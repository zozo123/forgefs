//! Kill a real concurrent ForgeFS workload, then prove cold recovery stays writable.

#![cfg(unix)]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const KILL_ATTEMPTS: usize = 4;
const KILL_WINDOW: Duration = Duration::from_secs(20);
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
    let deadline = Instant::now() + KILL_WINDOW;
    while Instant::now() < deadline {
        // Establish that the child is still live before trusting persistent
        // filesystem evidence it may have left behind.
        if child.try_wait().expect("poll benchmark child").is_some() {
            return false;
        }
        if forge_root.join("VERSION").is_file()
            && forge_root.join("keys/root.cap").is_file()
            && recursive_file_count(&objects) >= 12
        {
            return true;
        }
        thread::sleep(POLL_INTERVAL);
    }

    // A timeout is test infrastructure failure, but it must not leak a process
    // that holds the cell lock or consumes CI resources.
    let _ = child.kill();
    let _ = child.wait();
    false
}

fn spawn_active_bench(cell: &Path) -> Child {
    let mut bench = forge();
    bench
        .arg("--dir")
        .arg(cell)
        .arg("bench")
        .arg("--agents")
        .arg("256")
        .arg("--shared")
        .arg("128")
        .arg("--workers")
        .arg("16")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    bench.spawn().expect("spawn forge bench")
}

fn catch_real_sigkill(root: &Path) -> Option<PathBuf> {
    for attempt in 0..KILL_ATTEMPTS {
        let cell = root.join(format!("bench-cell-{attempt}"));
        let forge_root = cell.join(".forge");
        let mut child = spawn_active_bench(&cell);

        if !wait_for_active_publication(&mut child, &forge_root) {
            continue;
        }

        // The workload can finish in the tiny observation-to-signal race. That
        // is a missed attempt, not crash evidence; retry on a fresh cell.
        if child.kill().is_err() {
            let _ = child.wait();
            continue;
        }
        let status = child.wait().expect("wait killed benchmark");
        if status.signal() == Some(9) {
            return Some(cell);
        }
    }
    None
}

#[test]
fn cli_sigkill_live_concurrent_cell_reopens_and_accepts_new_work() {
    let d = tempdir().unwrap();

    // This is the shipped concurrent workload: private checkins plus shared-ref
    // stampede running inside a real `forge` process. Do not count a completed
    // run as crash evidence; retry until a real SIGKILL exit is observed.
    let cell = catch_real_sigkill(d.path())
        .expect("never caught an active concurrent ForgeFS workload with real SIGKILL");
    let forge_root = cell.join(".forge");
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
