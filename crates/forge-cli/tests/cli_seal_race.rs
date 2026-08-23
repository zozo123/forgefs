//! Process-level proof that a sealed tag has one immutable publisher.

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

fn make_commit(dir: &Path, cap: &Path, path: &str, text: &str) -> String {
    let mut open = authenticated(dir, cap);
    open.arg("session").arg("open").arg("--from=main");
    let ns = run(&mut open).trim().to_string();

    let mut write = authenticated(dir, cap);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg(path)
        .arg("--text")
        .arg(text);
    run(&mut write);

    let mut checkin = authenticated(dir, cap);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg(text);
    let result = run(&mut checkin);
    assert!(result.contains("updated"), "checkin did not update: {result}");
    format!("heads/agents/anon/{ns}")
}

fn tag_oid(dir: &Path, cap: &Path, tag: &str) -> String {
    let mut refs = authenticated(dir, cap);
    refs.arg("refs");
    let refs = run(&mut refs);
    let name = format!("tags/{tag}");
    refs.lines()
        .find(|line| line.split_whitespace().any(|field| field == name))
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or_else(|| panic!("missing {name} in refs:\n{refs}"))
        .to_string()
}

fn sealed_oid(out: &Output, tag: &str) -> Option<String> {
    if !out.status.success() {
        return None;
    }
    let prefix = format!("sealed tags/{tag} ");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
}

#[test]
fn cli_concurrent_sealers_publish_one_immutable_tag() {
    let d = tempdir().unwrap();
    let root = init(d.path());
    let left_ref = make_commit(d.path(), &root, "/left.txt", "left");
    let right_ref = make_commit(d.path(), &root, "/right.txt", "right");

    let barrier = Arc::new(Barrier::new(2));
    let mut launchers = Vec::new();
    for source in [left_ref.clone(), right_ref.clone()] {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            authenticated(&dir, &cap)
                .arg("seal")
                .arg(&source)
                .arg("--tag")
                .arg("race")
                .output()
                .expect("spawn racing forge seal")
        }));
    }

    let results = launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join seal launcher"))
        .collect::<Vec<_>>();
    let successes = results.iter().filter(|out| out.status.success()).count();
    assert_eq!(
        successes,
        1,
        "same tag must have one publisher; results={:?}",
        results
            .iter()
            .map(|out| {
                (
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                )
            })
            .collect::<Vec<_>>()
    );

    let winner_oid = results
        .iter()
        .find_map(|out| sealed_oid(out, "race"))
        .expect("successful sealer must print the snapshot oid");
    assert_eq!(
        tag_oid(d.path(), &root, "race"),
        winner_oid,
        "tag ref must name exactly the successful publisher's snapshot"
    );

    let mut verify = authenticated(d.path(), &root);
    verify.arg("verify").arg("race");
    let verified = run(&mut verify);
    assert!(verified.contains(&winner_oid), "verify returned {verified}");

    // Once published, even an explicit later attempt from the other source can
    // never replace the tag. Capture the winner before and after the retry.
    let before_retry = tag_oid(d.path(), &root, "race");
    let loser_source = if sealed_oid(&results[0], "race").is_some() {
        &right_ref
    } else {
        &left_ref
    };
    let retry = output(
        authenticated(d.path(), &root)
            .arg("seal")
            .arg(loser_source)
            .arg("--tag")
            .arg("race"),
    );
    assert!(!retry.status.success(), "sealed tag was overwritten");
    assert_eq!(tag_oid(d.path(), &root, "race"), before_retry);

    // Full mode also rehashes any orphan Snapshot the losing process may have
    // durably created before its metadata transaction lost the tag race.
    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
