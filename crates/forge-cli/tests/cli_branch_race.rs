//! Process-level proof that branch-name publication is create-once under contention.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
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

fn make_commit(dir: &Path, root: &Path, suffix: &str) -> (String, String) {
    let mut open = authenticated(dir, root);
    open.arg("session").arg("open").arg("--from=main");
    let ns = run(&mut open).trim().to_string();

    let mut write = authenticated(dir, root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg(format!("/{suffix}.txt"))
        .arg("--text")
        .arg(suffix);
    run(&mut write);

    let mut checkin = authenticated(dir, root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg(suffix);
    let result = run(&mut checkin);
    let oid = result
        .split_whitespace()
        .last()
        .expect("checkin must print commit oid")
        .to_string();
    assert!(result.contains("updated"), "unexpected checkin: {result}");
    (format!("heads/agents/anon/{ns}"), oid)
}

fn branch(dir: &Path, root: &Path, source: &str) -> Output {
    authenticated(dir, root)
        .arg("branch")
        .arg(source)
        .arg("heads/race")
        .output()
        .expect("spawn forge branch")
}

fn ref_oid(dir: &Path, root: &Path, name: &str) -> String {
    let mut refs = authenticated(dir, root);
    refs.arg("refs");
    let refs = run(&mut refs);
    refs.lines()
        .find(|line| line.split_whitespace().any(|field| field == name))
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or_else(|| panic!("missing ref {name} in:\n{refs}"))
        .to_string()
}

#[test]
fn cli_concurrent_branch_creation_has_one_immutable_first_publisher() {
    let d = tempdir().unwrap();
    let root = init(d.path());
    let (left_ref, left_oid) = make_commit(d.path(), &root, "left");
    let (right_ref, right_oid) = make_commit(d.path(), &root, "right");
    assert_ne!(left_oid, right_oid);

    let barrier = Arc::new(Barrier::new(2));
    let mut launchers = Vec::with_capacity(2);
    for source in [left_ref.clone(), right_ref.clone()] {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            branch(&dir, &cap, &source)
        }));
    }

    let results = launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join branch launcher"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| result.status.success())
            .count(),
        1,
        "same branch name must have exactly one publisher; results={:?}",
        results
            .iter()
            .map(|result| {
                (
                    result.status.code(),
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr),
                )
            })
            .collect::<Vec<_>>()
    );

    let winner = results
        .iter()
        .find(|result| result.status.success())
        .expect("one branch winner");
    let winner_oid = String::from_utf8_lossy(&winner.stdout)
        .split_whitespace()
        .last()
        .expect("branch must print oid")
        .to_string();
    assert!(winner_oid == left_oid || winner_oid == right_oid);
    assert_eq!(
        ref_oid(d.path(), &root, "heads/race"),
        winner_oid,
        "published branch must name exactly the winning source commit"
    );

    let loser_source = if winner_oid == left_oid {
        &right_ref
    } else {
        &left_ref
    };
    let retry = branch(d.path(), &root, loser_source);
    assert!(
        !retry.status.success(),
        "existing branch was overwritten by a later creator"
    );
    assert_eq!(ref_oid(d.path(), &root, "heads/race"), winner_oid);

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
