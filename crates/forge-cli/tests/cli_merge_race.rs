//! Process-level proof that concurrent integration never silently loses agent work.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const MERGERS: usize = 8;

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

fn init(dir: &Path) -> (PathBuf, PathBuf) {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    (
        dir.join(".forge/keys/root.cap"),
        dir.join(".forge/keys/integrator.cap"),
    )
}

fn make_agent_commit(dir: &Path, root: &Path, i: usize) -> String {
    let mut open = authenticated(dir, root);
    open.arg("session").arg("open").arg("--from=main");
    let ns = run(&mut open).trim().to_string();

    let mut write = authenticated(dir, root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg(format!("/agent-{i}.txt"))
        .arg("--text")
        .arg(format!("agent-{i}"));
    run(&mut write);

    let mut checkin = authenticated(dir, root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg(format!("agent-{i}"));
    let result = run(&mut checkin);
    assert!(result.contains("updated"), "agent checkin failed: {result}");
    format!("heads/agents/anon/{ns}")
}

fn merge(dir: &Path, integrator: &Path, source: &str) -> Output {
    authenticated(dir, integrator)
        .arg("merge")
        .arg("--into=main")
        .arg("--from")
        .arg(source)
        .output()
        .expect("spawn forge merge")
}

fn read(dir: &Path, root: &Path, ns: &str, path: &str) -> Vec<u8> {
    authenticated(dir, root)
        .arg("read")
        .arg("--ns")
        .arg(ns)
        .arg(path)
        .output()
        .map(|out| {
            assert!(
                out.status.success(),
                "read failed status={:?} stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        })
        .expect("spawn forge read")
}

#[test]
fn cli_concurrent_integrator_merges_preserve_every_agent_commit() {
    let d = tempdir().unwrap();
    let (root, integrator) = init(d.path());
    let sources = (0..MERGERS)
        .map(|i| make_agent_commit(d.path(), &root, i))
        .collect::<Vec<_>>();

    let barrier = Arc::new(Barrier::new(MERGERS));
    let mut launchers = Vec::with_capacity(MERGERS);
    for source in sources.iter().cloned() {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = integrator.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            merge(&dir, &cap, &source)
        }));
    }

    let results = launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join merge launcher"))
        .collect::<Vec<_>>();

    let mut updated = 0;
    let mut forked = 0;
    for result in &results {
        assert!(
            result.status.success(),
            "concurrent merge failed status={:?}\nstdout={}\nstderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        if stdout.contains("merged main") {
            updated += 1;
        } else if stdout.contains("merge forked") {
            forked += 1;
        } else {
            panic!("unexpected concurrent merge outcome: {stdout}");
        }
    }
    assert_eq!(updated + forked, MERGERS);
    assert!(updated >= 1, "at least one main CAS must succeed");

    // CAS losers are not discarded. They remain explicit fork refs until the
    // integrator chooses how to reconcile them.
    let mut refs = authenticated(d.path(), &root);
    refs.arg("refs");
    let refs = run(&mut refs);
    let fork_refs = refs
        .lines()
        .filter(|line| line.split_whitespace().any(|field| field.starts_with("forks/main/")))
        .count();
    assert_eq!(
        fork_refs, forked,
        "every concurrent CAS loser must have one explicit fork"
    );

    // Reconcile from the immutable original agent refs. Some may already be
    // ancestors of main; all invocations must remain successful and deterministic.
    for source in &sources {
        let result = merge(d.path(), &integrator, source);
        assert!(
            result.status.success(),
            "reconciliation failed for {source}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let mut open = authenticated(d.path(), &root);
    open.arg("session").arg("open").arg("--from=main");
    let ns = run(&mut open).trim().to_string();
    for i in 0..MERGERS {
        assert_eq!(
            read(
                d.path(),
                &root,
                &ns,
                &format!("/main/agent-{i}.txt")
            ),
            format!("agent-{i}").as_bytes(),
            "agent {i} disappeared during concurrent integration"
        );
    }

    let mut seal = authenticated(d.path(), &integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("merge-race")
        .arg("--attest");
    run(&mut seal);

    let mut verify = authenticated(d.path(), &root);
    verify.arg("verify").arg("merge-race");
    run(&mut verify);

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
