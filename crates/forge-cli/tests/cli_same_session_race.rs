
mod support;

use forge_api::Forge;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use support::output;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
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

fn checkin(dir: &Path, root: &Path, ns: &str, barrier: Option<&Path>) -> Output {
    let mut cmd = authenticated(dir, root);
    cmd.arg("checkin")
        .arg("--ns")
        .arg(ns)
        .arg("-m")
        .arg("same-session race");
    if let Some(barrier) = barrier {
        cmd.env("FORGEFS_TEST_CHECKIN_CAS_BARRIER", barrier);
    }
    output(&mut cmd)
}

fn fork_name(stdout: &str, requested: &str) -> Option<String> {
    let mut fields = stdout.split_whitespace();
    if fields.next()? != "forked" || fields.next()? != requested || fields.next()? != "->" {
        return None;
    }
    fields.next().map(str::to_string)
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
    let cas_barrier = d.path().join("checkin-cas-barrier");
    let mut launchers = Vec::with_capacity(2);
    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        let cas_barrier = cas_barrier.clone();
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        let ns = ns.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            checkin(&dir, &cap, &ns, Some(&cas_barrier))
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
    let forks = outputs
        .iter()
        .filter_map(|stdout| fork_name(stdout, &live_ref))
        .collect::<Vec<_>>();
    let noop = outputs
        .iter()
        .filter(|stdout| stdout.contains(&format!("noop {live_ref}")))
        .count();
    assert_eq!(
        updated, 1,
        "one and only one live-ref update is allowed: {outputs:?}"
    );
    assert_eq!(
        forks.len(),
        1,
        "the synchronized CAS loser must be an explicit fork: {outputs:?}"
    );
    assert_eq!(
        noop, 0,
        "the pre-CAS barrier must prevent a serialized noop: {outputs:?}"
    );

    let fork = &forks[0];
    assert!(
        fork.starts_with(&format!("forks/{live_ref}/")),
        "checkin reported an unexpected fork namespace: {fork}"
    );
    let reopened = Forge::open(d.path()).unwrap();
    let reopened_cap = reopened.root_cap().unwrap();
    let refs = reopened.refs(&reopened_cap).unwrap();
    assert_eq!(
        refs.iter().filter(|row| row.name == *fork).count(),
        1,
        "reported fork is not a unique durable ref: {fork}; refs={refs:?}"
    );
    drop(reopened_cap);
    drop(reopened);

    // The losing CAS retargets the persisted namespace to its durable fork.
    // That completed transition must still resolve the exact committed bytes.
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
    let third = checkin(d.path(), &root, &ns, None);
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
