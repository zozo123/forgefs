//! Real SIGKILL during checkin object publication before ref CAS.

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

fn fsck_full(dir: &Path, cap: &Path) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
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
        if child.try_wait().expect("poll checkin child").is_some() {
            return false;
        }
        thread::yield_now();
    }
    false
}

fn ref_oid(dir: &Path, cap: &Path, name: &str) -> String {
    let mut refs = authenticated(dir, cap);
    refs.arg("refs");
    let refs = run(&mut refs);
    refs.lines()
        .find(|line| line.split_whitespace().any(|field| field == name))
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or_else(|| panic!("missing ref {name} in:\n{refs}"))
        .to_string()
}

#[test]
fn cli_sigkill_during_checkin_preserves_ref_session_atomicity() {
    let d = tempdir().unwrap();
    let cell = d.path().join("cell");
    fs::create_dir(&cell).unwrap();
    let root = init(&cell);

    // Import a moderately wide tree so apply_overlay has a real tree object to
    // encode and publish. Unique bytes ensure the fixture is not deduplicated
    // down to one Blob, while staying bounded for PR CI.
    let source = d.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0u32..128 {
        fs::write(source.join(format!("f-{i:04}.bin")), i.to_le_bytes()).unwrap();
    }
    let mut import = authenticated(&cell, &root);
    import
        .arg("import")
        .arg(&source)
        .arg("--ref")
        .arg("heads/hot");
    run(&mut import);
    fsck_full(&cell, &root);

    let mut open = authenticated(&cell, &root);
    open.arg("session").arg("open").arg("--from=heads/hot");
    let ns = run(&mut open).trim().to_string();

    // Retarget the session root mount to the shared ref. checkin now performs a
    // real CAS on heads/hot while retaining the session's original pinned OID.
    let mut mount = authenticated(&cell, &root);
    mount
        .arg("mount")
        .arg("--ns")
        .arg(&ns)
        .arg("/")
        .arg("heads/hot")
        .arg("--rw");
    run(&mut mount);

    let mut write = authenticated(&cell, &root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/after.txt")
        .arg("--text")
        .arg("new overlay");
    run(&mut write);

    let before = ref_oid(&cell, &root, "heads/hot");
    let tmp = cell.join(".forge/tmp");
    assert!(!directory_has_file(&tmp), "tmp must start empty");

    let mut checkin = authenticated(&cell, &root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("kill during publish")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = checkin.spawn().expect("spawn forge checkin");

    // The first observed temp file belongs to checkin's batch publication
    // (new Tree / Contribution / Commit). Ref CAS occurs only after the whole
    // batch is durable, so SIGKILL here probes the object-before-metadata edge.
    assert!(
        wait_for_tmp(&mut child, &tmp),
        "checkin completed before its real CAS temp publication was observable"
    );
    child.kill().expect("SIGKILL checkin");
    let status = child.wait().expect("wait for killed checkin");
    assert_eq!(
        status.signal(),
        Some(9),
        "checkin was not terminated by SIGKILL"
    );

    fsck_full(&cell, &root);
    let after = ref_oid(&cell, &root, "heads/hot");

    // Retry the exact persisted session in a fresh process. If SIGKILL landed
    // before metadata CAS, the ref is unchanged and retry must update it. If
    // the SQL transaction committed just before the signal, ref + session
    // cleanup must have committed together and retry must be a noop.
    let mut retry = authenticated(&cell, &root);
    retry
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("retry after SIGKILL");
    let retry = run(&mut retry);
    if after == before {
        assert!(
            retry.contains("updated heads/hot"),
            "unchanged ref must retry as Updated, got: {retry}"
        );
    } else {
        assert!(
            retry.contains("noop heads/hot"),
            "advanced ref must imply atomic session completion, got: {retry}"
        );
    }
    fsck_full(&cell, &root);
}
