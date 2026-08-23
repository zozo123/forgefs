//! Real process-death recovery tests. No synthetic error return substitutes for SIGKILL.

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

fn open_session(dir: &Path, cap: &Path) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    run_text(&mut cmd).trim().to_string()
}

fn fsck_full(dir: &Path, cap: &Path) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

fn recursive_file_count(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(ty) if ty.is_dir() => recursive_file_count(&path),
                Ok(ty) if ty.is_file() => 1,
                _ => 0,
            }
        })
        .sum()
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

fn wait_while_running(child: &mut Child, timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        if child.try_wait().expect("poll child").is_some() {
            return false;
        }
        thread::yield_now();
    }
    false
}

fn kill_and_require_sigkill(child: &mut Child) {
    child.kill().expect("SIGKILL child");
    let status = child.wait().expect("wait for killed child");
    assert_eq!(
        status.signal(),
        Some(9),
        "process was not actually terminated by SIGKILL: {status:?}"
    );
}

#[test]
fn cli_sigkill_during_large_blob_put_reopens_without_dangling_state() {
    let d = tempdir().unwrap();
    let root = init(d.path());
    let ns = open_session(d.path(), &root);

    let mut refs_before = authenticated(d.path(), &root);
    refs_before.arg("refs");
    let refs_before = run_text(&mut refs_before);

    // A 64 MiB payload keeps the real temp-file write/fsync window open long
    // enough for the parent to observe `.forge/tmp` before sending SIGKILL.
    let payload = d.path().join("large.bin");
    fs::write(&payload, vec![0x5au8; 64 * 1024 * 1024]).unwrap();
    let tmp = d.path().join(".forge/tmp");
    assert!(!directory_has_file(&tmp), "tmp must start empty");

    let mut write = authenticated(d.path(), &root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/large.bin")
        .arg("--file")
        .arg(&payload)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = write.spawn().expect("spawn large forge write");

    assert!(
        wait_while_running(&mut child, Duration::from_secs(15), || directory_has_file(
            &tmp
        )),
        "never observed the real CAS temporary file while the writer was alive"
    );
    kill_and_require_sigkill(&mut child);

    // Cold-open through a new process. A killed put may leave an orphan durable
    // object or no object, but it may never publish a dangling ref.
    fsck_full(d.path(), &root);
    let mut refs_after = authenticated(d.path(), &root);
    refs_after.arg("refs");
    assert_eq!(
        run_text(&mut refs_after),
        refs_before,
        "SIGKILL changed a ref"
    );

    // The persisted session is still internally consistent. Depending on the
    // exact kill instant the overlay is either absent (noop) or fully durable
    // (updated); both outcomes must be retryable and fsck-clean.
    let mut retry = authenticated(d.path(), &root);
    retry
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("retry after SIGKILL");
    let result = run_text(&mut retry);
    assert!(
        result.contains("noop") || result.contains("updated"),
        "unexpected retry outcome after SIGKILL: {result}"
    );
    fsck_full(d.path(), &root);
}

#[test]
fn cli_sigkill_during_live_concurrent_workload_reopens_and_accepts_new_work() {
    let d = tempdir().unwrap();
    let cell = d.path().join("bench-cell");
    let forge_root = cell.join(".forge");
    let objects = forge_root.join("objects");

    // Drive the shipped benchmark binary so the killed process has active
    // concurrent private checkins and shared-ref CAS work, not an artificial
    // helper. We kill only after the object plane has visibly advanced beyond
    // bootstrap while the process is still alive.
    let mut bench = forge();
    bench
        .arg("--dir")
        .arg(&cell)
        .arg("bench")
        .arg("--agents")
        .arg("256")
        .arg("--shared")
        .arg("128")
        .arg("--workers")
        .arg("16")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = bench.spawn().expect("spawn forge bench");

    assert!(
        wait_while_running(&mut child, Duration::from_secs(20), || {
            forge_root.join("VERSION").is_file() && recursive_file_count(&objects) >= 12
        }),
        "benchmark finished or stalled before observable concurrent publication"
    );
    kill_and_require_sigkill(&mut child);

    let root = forge_root.join("keys/root.cap");
    assert!(
        root.is_file(),
        "killed benchmark never published a valid repository"
    );
    fsck_full(&cell, &root);

    // Recovery is not merely readable: a fresh binary must be able to create
    // and commit new work after SQLite/WAL and object-store recovery.
    let ns = open_session(&cell, &root);
    let mut write = authenticated(&cell, &root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/after-kill.txt")
        .arg("--text")
        .arg("recovered");
    run(&mut write);
    let mut checkin = authenticated(&cell, &root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("post-SIGKILL recovery");
    let result = run_text(&mut checkin);
    assert!(
        result.contains("updated"),
        "post-kill checkin failed: {result}"
    );
    fsck_full(&cell, &root);
}
