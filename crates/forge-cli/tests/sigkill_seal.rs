//! Real SIGKILL during seal publication: tag is absent or fully verifiable, never partial.

#![cfg(unix)]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> String {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("forge stdout is UTF-8")
}

fn init(dir: &Path) -> PathBuf {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    dir.join(".forge/keys/root.cap")
}

fn directory_has_file(root: &Path) -> bool {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_type().map(|ty| ty.is_file()).unwrap_or(false))
        })
        .unwrap_or(false)
}

fn wait_for_tmp(child: &mut Child, tmp: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if directory_has_file(tmp) {
            return true;
        }
        if child.try_wait().expect("poll seal child").is_some() {
            return false;
        }
        thread::yield_now();
    }
    false
}

fn fsck_full(dir: &Path, cap: &Path) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

#[test]
fn cli_sigkill_during_seal_is_absent_or_fully_verifiable() {
    let d = tempdir().unwrap();
    let cell = d.path().join("cell");
    fs::create_dir(&cell).unwrap();
    let root = init(&cell);

    // Give seal a nontrivial provenance walk without requiring hundreds of CLI
    // processes. Unique bytes create unique Blob OIDs; import publishes one real
    // committed tree through the shipped binary before the seal race begins.
    let source = d.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0u32..256 {
        fs::write(source.join(format!("f-{i:04}.bin")), i.to_le_bytes()).unwrap();
    }
    let mut import = authenticated(&cell, &root);
    import
        .arg("import")
        .arg(&source)
        .arg("--ref")
        .arg("heads/import");
    run(&mut import);
    fsck_full(&cell, &root);

    let tmp = cell.join(".forge/tmp");
    assert!(!directory_has_file(&tmp), "tmp must start empty");

    let mut seal = authenticated(&cell, &root);
    seal.arg("seal")
        .arg("heads/import")
        .arg("--tag")
        .arg("killseal")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = seal.spawn().expect("spawn forge seal");

    assert!(
        wait_for_tmp(&mut child, &tmp),
        "seal completed before its real CAS temp publication could be observed"
    );
    child.kill().expect("SIGKILL seal");
    let status = child.wait().expect("wait for killed seal");
    assert_eq!(status.signal(), Some(9), "seal was not killed by SIGKILL");

    // SQLite transactionality + write-once object publication must make the
    // metadata outcome binary. Full fsck catches any object corruption/orphan
    // references. The tag may be absent, or it may have committed just before
    // SIGKILL; if present it must verify completely.
    fsck_full(&cell, &root);
    let mut refs = authenticated(&cell, &root);
    refs.arg("refs");
    let refs = run(&mut refs);
    if refs.contains("tags/killseal") {
        let mut verify = authenticated(&cell, &root);
        verify.arg("verify").arg("killseal");
        run(&mut verify);
    } else {
        let mut verify = authenticated(&cell, &root);
        verify.arg("verify").arg("killseal");
        let absent = output(&mut verify);
        assert_eq!(
            absent.status.code(),
            Some(1),
            "absent tag returned an unexpected status"
        );

        // Retry after the crash must complete the seal and verify in fresh
        // processes; this also proves the repository is still writable.
        let mut retry = authenticated(&cell, &root);
        retry
            .arg("seal")
            .arg("heads/import")
            .arg("--tag")
            .arg("killseal")
            .arg("--attest");
        run(&mut retry);
    }
    fsck_full(&cell, &root);
}
