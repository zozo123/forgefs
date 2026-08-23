//! Kill a real concurrent ForgeFS workload, then prove cold recovery stays writable.

#![cfg(unix)]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
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
        .map(|entry| match entry.file_type() {
            Ok(ty) if ty.is_dir() => recursive_file_count(&entry.path()),
            Ok(ty) if ty.is_file() => 1,
            _ => 0,
        })
        .sum()
}

fn wait_for_active_publication(child: &mut Child, forge_root: &Path) -> bool {
    let objects = forge_root.join("objects");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if child.try_wait().expect("poll benchmark child").is_some() {
            return false;
        }
        if forge_root.join("VERSION").is_file() && recursive_file_count(&objects) >= 12 {
            return true;
        }
        thread::yield_now();
    }
    false
}

#[test]
fn cli_sigkill_live_concurrent_cell_reopens_and_accepts_new_work() {
    let d = tempdir().unwrap();
    let cell = d.path().join("bench-cell");
    let forge_root = cell.join(".forge");

    // This is the shipped concurrent workload: private checkins plus shared-ref
    // stampede running inside a real `forge` process. Kill only after the cell
    // is valid and immutable object publication has advanced beyond bootstrap.
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
        wait_for_active_publication(&mut child, &forge_root),
        "benchmark completed or stalled before observable concurrent publication"
    );
    child.kill().expect("SIGKILL active benchmark");
    let status = child.wait().expect("wait killed benchmark");
    assert_eq!(
        status.signal(),
        Some(9),
        "benchmark was not actually terminated by SIGKILL: {status:?}"
    );

    let root = forge_root.join("keys/root.cap");
    assert!(
        root.is_file(),
        "killed benchmark did not leave a valid cell"
    );

    // Cold SQLite/WAL + object-store recovery must be structurally clean.
    fsck_full(&cell, &root);

    // Recovery is stronger than read-only fsck: a fresh binary can still open
    // a session, publish new immutable data, advance a ref, and fsck again.
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
