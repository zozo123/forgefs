//! Process-level agent handoff through the shipped inbox CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

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

fn open_session(dir: &Path, cap: &str, from: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from").arg(from);
    run_text(&mut cmd).trim().to_string()
}

#[test]
fn cli_inbox_push_list_and_continue_from_snapshot() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let integrator_path = d.path().join(".forge/keys/integrator.cap");
    let integrator = integrator_path.to_str().unwrap();

    let producer = open_session(d.path(), root, "main");
    let mut write = authenticated(d.path(), root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&producer)
        .arg("/handoff.txt")
        .arg("--text")
        .arg("sealed handoff");
    run(&mut write);

    let mut checkin = authenticated(d.path(), root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&producer)
        .arg("-m")
        .arg("handoff payload");
    assert!(run_text(&mut checkin).contains("updated"));

    let mut merge = authenticated(d.path(), integrator);
    merge
        .arg("merge")
        .arg("--into=main")
        .arg("--from")
        .arg(format!("heads/agents/anon/{producer}"));
    run(&mut merge);

    let mut seal = authenticated(d.path(), integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("handoff-v1")
        .arg("--attest");
    let sealed = run_text(&mut seal);
    let snapshot_oid = sealed
        .lines()
        .find_map(|line| line.strip_prefix("sealed tags/handoff-v1 "))
        .expect("seal prints snapshot oid")
        .to_string();

    let mut grant_bob = authenticated(d.path(), root);
    grant_bob
        .arg("grant")
        .arg("--ops")
        .arg("read,write,branch")
        .arg("--ref")
        .arg("tags/*,inbox/bob/*,heads/agents/bob/*")
        .arg("--agent")
        .arg("bob");
    let bob = run_text(&mut grant_bob).trim().to_string();

    let mut push = authenticated(d.path(), root);
    push.arg("inbox")
        .arg("push")
        .arg("--to")
        .arg("bob")
        .arg("--snapshot")
        .arg("tags/handoff-v1");
    let pushed = run_text(&mut push);
    let mut fields = pushed.split_whitespace();
    assert_eq!(fields.next(), Some("pushed"));
    let inbox_ref = fields.next().expect("inbox ref").to_string();
    let pushed_oid = fields.next().expect("snapshot oid");
    assert!(inbox_ref.starts_with("inbox/bob/"), "{pushed}");
    assert_eq!(pushed_oid, snapshot_oid);
    assert!(fields.next().is_none(), "unexpected push output: {pushed}");

    let mut list = authenticated(d.path(), &bob);
    list.arg("inbox").arg("list");
    let listed = run_text(&mut list);
    assert!(
        listed
            .lines()
            .any(|line| line == format!("{inbox_ref} {snapshot_oid}")),
        "Bob cannot see handoff: {listed}"
    );

    // The handoff is not just a notification: Bob can pin the sealed snapshot
    // as a new session base and continue work under Bob's own live-ref scope.
    let bob_ns = open_session(d.path(), &bob, &inbox_ref);
    let mut read = authenticated(d.path(), &bob);
    read.arg("read")
        .arg("--ns")
        .arg(&bob_ns)
        .arg("/handoff.txt");
    assert_eq!(run(&mut read), b"sealed handoff");

    let mut bob_write = authenticated(d.path(), &bob);
    bob_write
        .arg("write")
        .arg("--ns")
        .arg(&bob_ns)
        .arg("/bob.txt")
        .arg("--text")
        .arg("continued by bob");
    run(&mut bob_write);
    let mut bob_checkin = authenticated(d.path(), &bob);
    bob_checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&bob_ns)
        .arg("-m")
        .arg("bob continuation");
    assert!(run_text(&mut bob_checkin).contains("updated"));

    // Bob can read the source tag but has write authority only for his own
    // inbox/live-ref prefixes. The CLI must not turn a recipient into a relay
    // with authority over another agent's inbox.
    let mut cross_push = authenticated(d.path(), &bob);
    cross_push
        .arg("inbox")
        .arg("push")
        .arg("--to")
        .arg("alice")
        .arg("--snapshot")
        .arg("tags/handoff-v1");
    let cross_push = output(&mut cross_push);
    assert_eq!(cross_push.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&cross_push.stderr).contains("denied"),
        "unexpected cross-inbox error: {}",
        String::from_utf8_lossy(&cross_push.stderr)
    );

    let mut fsck = authenticated(d.path(), root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
