#![cfg(unix)]

//! Real process-death tests. A child is killed only after the test observes an
//! actual CAS temporary file, so these are publication-window failures rather
//! than sleep-based timing guesses.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const KILL_WINDOW: Duration = Duration::from_secs(3);

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &str) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_text(cmd: &mut Command) -> String {
    String::from_utf8(run(cmd)).expect("forge stdout is UTF-8")
}

fn init(dir: &Path) -> PathBuf {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    dir.join(".forge/keys/root.cap")
}

fn open_session(dir: &Path, cap: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    run_text(&mut cmd).trim().to_string()
}

fn fsck_full(dir: &Path, cap: &str) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

fn tmp_has_file(tmp: &Path) -> bool {
    fs::read_dir(tmp)
        .expect("read .forge/tmp")
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().map(|ty| ty.is_file()).unwrap_or(false))
}

/// Kill only while a real CAS temp file is visible. `None` means the child
/// completed before this attempt caught the publication window; callers retry
/// with a fresh object/tag rather than counting that as crash evidence.
fn sigkill_when_tmp_visible(child: &mut Child, tmp: &Path) -> Option<()> {
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return None;
        }
        if tmp_has_file(tmp) {
            if child.kill().is_err() {
                let _ = child.wait();
                return None;
            }
            let status = child.wait().expect("wait killed child");
            assert_eq!(
                status.signal(),
                Some(9),
                "child was not terminated by SIGKILL: {status:?}"
            );
            return Some(());
        }
        if started.elapsed() > KILL_WINDOW {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child remained live without exposing a CAS temp file");
        }
        std::thread::yield_now();
    }
}

fn spawn_quiet(cmd: &mut Command) -> Child {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crash target")
}

#[test]
fn cli_sigkill_during_large_blob_put_leaves_repository_consistent() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let ns = open_session(d.path(), root);
    let tmp = d.path().join(".forge/tmp");
    let payload_path = d.path().join("large.bin");
    let mut killed = false;

    for attempt in 0u8..8 {
        let mut payload = vec![attempt; 8 * 1024 * 1024];
        payload[0] = attempt.wrapping_add(1);
        fs::write(&payload_path, payload).unwrap();
        assert!(!tmp_has_file(&tmp), "stale temp before write attempt");

        let mut write = authenticated(d.path(), root);
        write
            .arg("write")
            .arg("--ns")
            .arg(&ns)
            .arg(format!("/large-{attempt}.bin"))
            .arg("--file")
            .arg(&payload_path);
        let mut child = spawn_quiet(&mut write);
        if sigkill_when_tmp_visible(&mut child, &tmp).is_some() {
            killed = true;
            break;
        }
    }

    assert!(killed, "never caught a real blob publication window");
    fsck_full(d.path(), root);

    let mut refs = authenticated(d.path(), root);
    refs.arg("refs");
    assert!(run_text(&mut refs).contains("main"));
}

#[test]
fn cli_sigkill_during_checkin_is_retryable_and_never_dangles() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let tmp = d.path().join(".forge/tmp");
    let mut victim = None;

    for attempt in 0..32 {
        let ns = open_session(d.path(), root);
        let mut write = authenticated(d.path(), root);
        write
            .arg("write")
            .arg("--ns")
            .arg(&ns)
            .arg(format!("/checkin-{attempt}.txt"))
            .arg("--text")
            .arg(format!("attempt-{attempt}"));
        run(&mut write);
        assert!(!tmp_has_file(&tmp), "stale temp before checkin attempt");

        let mut checkin = authenticated(d.path(), root);
        checkin
            .arg("checkin")
            .arg("--ns")
            .arg(&ns)
            .arg("-m")
            .arg("kill during checkin");
        let mut child = spawn_quiet(&mut checkin);
        if sigkill_when_tmp_visible(&mut child, &tmp).is_some() {
            victim = Some(ns);
            break;
        }
    }

    let victim = victim.expect("never caught a real checkin publication window");
    // The child died while an immutable object was still in the CAS temp path,
    // strictly before checkin can perform its metadata CAS. Orphans are safe;
    // a committed ref must never name a missing/corrupt object.
    fsck_full(d.path(), root);

    let mut retry = authenticated(d.path(), root);
    retry
        .arg("checkin")
        .arg("--ns")
        .arg(&victim)
        .arg("-m")
        .arg("retry after SIGKILL");
    let retry = run_text(&mut retry);
    assert!(
        retry.contains("updated"),
        "unexpected retry result: {retry}"
    );
    fsck_full(d.path(), root);
}

#[test]
fn cli_sigkill_during_seal_is_absent_or_fully_verifiable() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let integrator_path = d.path().join(".forge/keys/integrator.cap");
    let integrator = integrator_path.to_str().unwrap();
    let tmp = d.path().join(".forge/tmp");
    let mut killed_tag = None;

    for attempt in 0..32 {
        let tag = format!("kill-{attempt}");
        assert!(!tmp_has_file(&tmp), "stale temp before seal attempt");
        let mut seal = authenticated(d.path(), integrator);
        seal.arg("seal").arg("main").arg("--tag").arg(&tag);
        let mut child = spawn_quiet(&mut seal);
        if sigkill_when_tmp_visible(&mut child, &tmp).is_some() {
            killed_tag = Some(tag);
            break;
        }
    }

    let tag = killed_tag.expect("never caught a real seal publication window");
    fsck_full(d.path(), root);

    let mut refs = authenticated(d.path(), root);
    refs.arg("refs");
    let refs = run_text(&mut refs);
    if refs.contains(&format!("tags/{tag}")) {
        let mut verify = authenticated(d.path(), root);
        verify.arg("verify").arg(&tag);
        run(&mut verify);
    }
}
