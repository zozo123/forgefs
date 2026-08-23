//! Process-level capability boundary tests using the shipped `forge` binary.

use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn forge");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    stdout
}

fn assert_denied(cmd: &mut Command) {
    let out = cmd.output().expect("spawn forge");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected denied exit\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("denied:"),
        "expected capability denial\nstdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn cli_attenuated_cap_is_not_root() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let dir = d.path().to_str().unwrap();
    let root = d.path().join(".forge/keys/root.cap");
    let root = root.to_str().unwrap();

    let alice = run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "grant",
        "--ops",
        "read,write,branch",
        "--ref",
        "main,heads/agents/alice/*",
        "--agent",
        "alice",
    ]));
    let alice = alice.trim().to_string();
    let bob = run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "grant",
        "--ops",
        "read,write,branch",
        "--ref",
        "main,heads/agents/bob/*",
        "--agent",
        "bob",
    ]));
    let bob = bob.trim().to_string();

    let alice_ns = run(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "session",
        "open",
        "--from=main",
    ]));
    let alice_ns = alice_ns.trim().to_string();
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "write",
        "--ns",
        &alice_ns,
        "/alice.txt",
        "--text",
        "alice",
    ]));
    let checked_in = run(forge().args([
        "--dir", dir, "--cap", &alice, "checkin", "--ns", &alice_ns, "-m", "alice",
    ]));
    assert!(checked_in.contains("updated"), "{checked_in}");

    let bob_ns = run(forge().args([
        "--dir",
        dir,
        "--cap",
        &bob,
        "session",
        "open",
        "--from=main",
    ]));
    let bob_ns = bob_ns.trim().to_string();
    run(forge().args([
        "--dir", dir, "--cap", &bob, "write", "--ns", &bob_ns, "/bob.txt", "--text", "bob",
    ]));
    run(forge().args([
        "--dir", dir, "--cap", &bob, "checkin", "--ns", &bob_ns, "-m", "bob",
    ]));

    let alice_ref = format!("heads/agents/alice/{alice_ns}");
    let bob_ref = format!("heads/agents/bob/{bob_ns}");
    let visible = run(forge().args(["--dir", dir, "--cap", &alice, "refs"]));
    assert!(visible.contains("main"), "{visible}");
    assert!(visible.contains(&alice_ref), "{visible}");
    assert!(!visible.contains(&bob_ref), "{visible}");

    assert_denied(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "seal",
        "main",
        "--tag",
        "forbidden",
    ]));
    assert_denied(forge().args(["--dir", dir, "--cap", &alice, "fsck", "--full"]));
    assert_denied(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "merge",
        "--into=main",
        "--from",
        &bob_ref,
    ]));
    assert_denied(forge().args(["--dir", dir, "--cap", &alice, "grant", "--ops", "read"]));
    assert_denied(forge().args([
        "--dir", dir, "--cap", &alice, "session", "open", "--from", &bob_ref,
    ]));
}
