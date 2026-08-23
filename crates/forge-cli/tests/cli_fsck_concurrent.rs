//! Process-level proof that full scrub is safe while the cell is actively mutating.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHECKINS: usize = 8;
const PUBLICATION_WINDOW: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

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

fn open_session(dir: &Path, cap: &Path) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    run(&mut cmd).trim().to_string()
}

fn fsck(dir: &Path, cap: &Path) -> Output {
    authenticated(dir, cap)
        .arg("fsck")
        .arg("--full")
        .output()
        .expect("spawn forge fsck")
}

fn assert_success(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tmp_has_file(tmp: &Path) -> bool {
    fs::read_dir(tmp)
        .expect("read .forge/tmp")
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().map(|ty| ty.is_file()).unwrap_or(false))
}

fn wait_for_live_tmp(child: &mut Child, tmp: &Path) -> bool {
    let deadline = Instant::now() + PUBLICATION_WINDOW;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll writer").is_some() {
            return false;
        }
        if tmp_has_file(tmp) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("writer stayed live without exposing a CAS temporary file");
}

#[test]
fn cli_full_fsck_succeeds_during_live_blob_publication() {
    let d = tempdir().unwrap();
    let root = init(d.path());
    let tmp = d.path().join(".forge/tmp");
    let payload_path = d.path().join("payload.bin");
    let mut caught_session = None;

    for attempt in 0u8..8 {
        let ns = open_session(d.path(), &root);
        let mut payload = vec![attempt; 8 * 1024 * 1024];
        payload[0] = attempt.wrapping_add(1);
        fs::write(&payload_path, payload).unwrap();
        assert!(!tmp_has_file(&tmp), "stale temp before writer attempt");

        let mut write = authenticated(d.path(), &root);
        write
            .arg("write")
            .arg("--ns")
            .arg(&ns)
            .arg(format!("/live-{attempt}.bin"))
            .arg("--file")
            .arg(&payload_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = write.spawn().expect("spawn live writer");
        if !wait_for_live_tmp(&mut child, &tmp) {
            continue;
        }

        // The writer is alive and its object is still in the unpublished temp
        // path when scrub starts. A named object must therefore be either absent
        // from the scan or complete/hash-valid; partial bytes must never leak.
        let scrub = fsck(d.path(), &root);
        assert_success(&scrub, "concurrent full fsck");
        let status = child.wait().expect("wait live writer");
        assert!(status.success(), "writer failed after concurrent fsck");
        caught_session = Some(ns);
        break;
    }

    let ns = caught_session.expect("never caught a live Blob publication window");
    let mut checkin = authenticated(d.path(), &root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("after concurrent fsck");
    let checked_in = run(&mut checkin);
    assert!(checked_in.contains("updated"), "{checked_in}");
    assert_success(&fsck(d.path(), &root), "final full fsck");
}

#[test]
fn cli_full_fsck_succeeds_during_concurrent_checkin_transactions() {
    let d = tempdir().unwrap();
    let root = init(d.path());
    let sessions = (0..CHECKINS)
        .map(|i| {
            let ns = open_session(d.path(), &root);
            let mut write = authenticated(d.path(), &root);
            write
                .arg("write")
                .arg("--ns")
                .arg(&ns)
                .arg(format!("/txn-{i}.txt"))
                .arg("--text")
                .arg(format!("txn-{i}"));
            run(&mut write);
            ns
        })
        .collect::<Vec<_>>();

    let barrier = Arc::new(Barrier::new(CHECKINS + 1));
    let mut launchers = Vec::with_capacity(CHECKINS);
    for ns in sessions {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            authenticated(&dir, &cap)
                .arg("checkin")
                .arg("--ns")
                .arg(&ns)
                .arg("-m")
                .arg("concurrent transaction")
                .output()
                .expect("spawn checkin")
        }));
    }

    barrier.wait();
    let scrub = fsck(d.path(), &root);
    assert_success(&scrub, "fsck racing checkin transactions");

    for launcher in launchers {
        let result = launcher.join().expect("join checkin launcher");
        assert_success(&result, "concurrent checkin");
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("updated"),
            "unexpected private checkin result: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
    assert_success(&fsck(d.path(), &root), "final transaction full fsck");
}
