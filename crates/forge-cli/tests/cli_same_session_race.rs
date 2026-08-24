//! Process-level proof that double-checkin of one namespace has one live-ref winner.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(cmd: &mut Command) -> Self {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        Self(Some(cmd.spawn().expect("spawn forge")))
    }

    fn wait(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.0.as_mut().expect("guard owns child");
            if child.try_wait().expect("poll forge child").is_some() {
                return self.collect();
            }
            if Instant::now() >= deadline {
                let mut child = self.0.take().expect("guard owns child");
                let _ = child.kill();
                let output = collect_output(child);
                panic!(
                    "forge child exceeded {timeout:?}\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn collect(&mut self) -> Output {
        collect_output(self.0.take().expect("guard owns child"))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn collect_output(child: Child) -> Output {
    child.wait_with_output().expect("collect forge output")
}

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn output(cmd: &mut Command) -> Output {
    ChildGuard::spawn(cmd).wait(PROCESS_TIMEOUT)
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

fn checkin(dir: &Path, root: &Path, ns: &str) -> Output {
    let mut cmd = authenticated(dir, root);
    cmd.arg("checkin")
        .arg("--ns")
        .arg(ns)
        .arg("-m")
        .arg("same-session race");
    output(&mut cmd)
}

#[test]
fn cli_two_processes_checking_in_one_session_have_one_live_ref_winner() {
    let d = tempdir().unwrap();
    let root = init(d.path());

    let mut open = authenticated(d.path(), &root);
    open.arg("session").arg("open").arg("--from=main");
    let ns = run(&mut open).trim().to_string();
    let live_ref = format!("heads/agents/anon/{ns}");

    let mut write = authenticated(d.path(), &root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/once.txt")
        .arg("--text")
        .arg("exactly once data");
    run(&mut write);

    let barrier = Arc::new(Barrier::new(2));
    let mut launchers = Vec::with_capacity(2);
    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        let ns = ns.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            checkin(&dir, &cap, &ns)
        }));
    }

    let results = launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join checkin launcher"))
        .collect::<Vec<_>>();
    for result in &results {
        assert!(
            result.status.success(),
            "same-session checkin failed status={:?}\nstdout={}\nstderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let outputs = results
        .iter()
        .map(|result| String::from_utf8_lossy(&result.stdout).into_owned())
        .collect::<Vec<_>>();
    let updated = outputs
        .iter()
        .filter(|stdout| stdout.contains(&format!("updated {live_ref}")))
        .count();
    let forked = outputs
        .iter()
        .filter(|stdout| stdout.contains(&format!("forked {live_ref} -> forks/{live_ref}/")))
        .count();
    let noop = outputs
        .iter()
        .filter(|stdout| stdout.contains(&format!("noop {live_ref}")))
        .count();
    assert_eq!(
        updated, 1,
        "one and only one live-ref update is allowed: {outputs:?}"
    );
    assert_eq!(
        updated + forked + noop,
        2,
        "the loser must be an explicit fork or an idempotent noop: {outputs:?}"
    );
    assert!(
        forked <= 1 && noop <= 1,
        "unexpected duplicate outcome: {outputs:?}"
    );

    // Regardless of whether the second process got far enough to publish an
    // explicit fork or observed the already-cleared overlay as a noop, the
    // persisted namespace must still resolve the complete committed bytes.
    let mut read = authenticated(d.path(), &root);
    read.arg("read").arg("--ns").arg(&ns).arg("/once.txt");
    let read = output(&mut read);
    assert!(
        read.status.success(),
        "session became unreadable after race"
    );
    assert_eq!(read.stdout, b"exactly once data");

    // A later invocation is idempotent: the overlay was cleared atomically by
    // the successful session transition and cannot be published again.
    let third = checkin(d.path(), &root, &ns);
    assert!(third.status.success());
    assert!(
        String::from_utf8_lossy(&third.stdout).contains("noop"),
        "completed session re-published its overlay: {}",
        String::from_utf8_lossy(&third.stdout)
    );

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
