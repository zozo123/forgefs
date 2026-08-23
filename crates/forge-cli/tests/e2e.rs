//! Process-level e2e: real `forge` binaries, required --cap, parallel checkin.

use forge_api::RAW_MERGE_RESOLUTION_DISABLED;
use std::path::Path;
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

fn run_denied(cmd: &mut Command) -> String {
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
    stderr
}

fn assert_repo_operational(dir: &Path) {
    let dir = dir.to_str().unwrap();
    let cap = Path::new(dir).join(".forge/keys/root.cap");
    let cap = cap.to_str().unwrap();
    let refs = run(forge().args(["--dir", dir, "--cap", cap, "refs"]));
    assert!(refs.contains("main"), "{refs}");
    run(forge().args(["--dir", dir, "--cap", cap, "fsck", "--full"]));
}

#[test]
fn cli_init_crash_matrix_never_publishes_a_partial_repository() {
    let before_publication = [
        "staging-created",
        "directories-created",
        "keys-written",
        "catalog-created",
        "initial-objects-written",
        "main-ref-written",
        "version-written",
        "staging-durable",
    ];

    for point in before_publication {
        let d = tempdir().unwrap();
        let output = forge()
            .arg("init")
            .current_dir(d.path())
            .env("FORGEFS_TEST_INIT_CRASH_AFTER", point)
            .output()
            .expect("spawn crashing forge init");
        assert_eq!(output.status.code(), Some(86), "phase={point}");
        assert!(
            !d.path().join(".forge").exists(),
            "phase={point} published a partial repository"
        );

        // A fresh process can safely initialize despite the orphaned sibling
        // staging directory, then open and verify the complete repository.
        run(forge().arg("init").current_dir(d.path()));
        assert_repo_operational(d.path());
    }

    // Rename is the visibility linearization point. A process crash either
    // immediately after rename or after the parent barrier must expose a
    // complete repository, never one that a retry overwrites. A cold open also
    // re-proves the parent edge left unforced by the `published` case.
    for point in ["published", "parent-durable"] {
        let d = tempdir().unwrap();
        let output = forge()
            .arg("init")
            .current_dir(d.path())
            .env("FORGEFS_TEST_INIT_CRASH_AFTER", point)
            .output()
            .expect("spawn crashing forge init");
        assert_eq!(output.status.code(), Some(86), "phase={point}");
        assert_eq!(
            std::fs::read(d.path().join(".forge/VERSION")).unwrap(),
            b"1\n",
            "phase={point}"
        );
        assert_repo_operational(d.path());

        let retry = forge().arg("init").current_dir(d.path()).output().unwrap();
        assert_eq!(retry.status.code(), Some(1), "phase={point}");
    }
}

#[test]
fn cli_requires_cap() {
    let d = tempdir().unwrap();
    run(forge().args(["init"]).current_dir(d.path()));
    let out = forge()
        .args(["--dir", d.path().to_str().unwrap(), "refs"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no ambient root"), "{err}");
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

    let a = run(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "session",
        "open",
        "--from=main",
    ]));
    let a = a.trim().to_string();
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "write",
        "--ns",
        &a,
        "/alice.txt",
        "--text",
        "alice",
    ]));
    let checked_in = run(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "checkin",
        "--ns",
        &a,
        "-m",
        "alice",
    ]));
    assert!(checked_in.contains("updated"), "{checked_in}");

    let b = run(forge().args([
        "--dir",
        dir,
        "--cap",
        &bob,
        "session",
        "open",
        "--from=main",
    ]));
    let b = b.trim().to_string();
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        &bob,
        "write",
        "--ns",
        &b,
        "/bob.txt",
        "--text",
        "bob",
    ]));
    run(forge().args([
        "--dir", dir, "--cap", &bob, "checkin", "--ns", &b, "-m", "bob",
    ]));

    let alice_ref = format!("heads/agents/alice/{a}");
    let bob_ref = format!("heads/agents/bob/{b}");
    let visible = run(forge().args(["--dir", dir, "--cap", &alice, "refs"]));
    assert!(visible.contains("main"), "{visible}");
    assert!(visible.contains(&alice_ref), "{visible}");
    assert!(!visible.contains(&bob_ref), "{visible}");

    run_denied(forge().args([
        "--dir", dir, "--cap", &alice, "seal", "main", "--tag", "forbidden",
    ]));
    run_denied(forge().args(["--dir", dir, "--cap", &alice, "fsck", "--full"]));
    run_denied(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "merge",
        "--into=main",
        "--from",
        &bob_ref,
    ]));
    run_denied(forge().args([
        "--dir", dir, "--cap", &alice, "grant", "--ops", "read",
    ]));
    run_denied(forge().args([
        "--dir",
        dir,
        "--cap",
        &alice,
        "session",
        "open",
        "--from",
        &bob_ref,
    ]));
}

#[test]
fn cli_two_agents_merge_seal_export() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let cap = d.path().join(".forge/keys/root.cap");
    let integ = d.path().join(".forge/keys/integrator.cap");
    let cap_s = cap.to_str().unwrap();
    let dir = d.path().to_str().unwrap();

    let a = run(forge().args([
        "--dir",
        dir,
        "--cap",
        cap_s,
        "session",
        "open",
        "--from=main",
    ]));
    let b = run(forge().args([
        "--dir",
        dir,
        "--cap",
        cap_s,
        "session",
        "open",
        "--from=main",
    ]));
    let a = a.trim();
    let b = b.trim();
    run(forge().args([
        "--dir", dir, "--cap", cap_s, "write", "--ns", a, "/a.txt", "--text", "alice",
    ]));
    run(forge().args([
        "--dir", dir, "--cap", cap_s, "write", "--ns", b, "/b.txt", "--text", "bob",
    ]));
    let ca = run(forge().args([
        "--dir", dir, "--cap", cap_s, "checkin", "--ns", a, "-m", "a",
    ]));
    let cb = run(forge().args([
        "--dir", dir, "--cap", cap_s, "checkin", "--ns", b, "-m", "b",
    ]));
    assert!(ca.contains("updated"), "{ca}");
    assert!(cb.contains("updated"), "{cb}");

    let live_a = format!("heads/agents/anon/{a}");
    let live_b = format!("heads/agents/anon/{b}");
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        integ.to_str().unwrap(),
        "merge",
        "--into=main",
        "--from",
        &live_a,
    ]));
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        integ.to_str().unwrap(),
        "merge",
        "--into=main",
        "--from",
        &live_b,
    ]));
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        integ.to_str().unwrap(),
        "seal",
        "main",
        "--tag",
        "v1.0",
        "--attest",
    ]));
    run(forge().args(["--dir", dir, "--cap", cap_s, "verify", "v1.0"]));
    let tar = d.path().join("v1.0.tar");
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        cap_s,
        "export",
        "tags/v1.0",
        "-o",
        tar.to_str().unwrap(),
    ]));
    assert!(tar.exists());
    assert!(tar.metadata().unwrap().len() > 64);
}

#[test]
fn cli_parallel_checkin_processes() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let cap = d.path().join(".forge/keys/root.cap");
    let dir = d.path().to_str().unwrap().to_string();
    let cap_s = cap.to_str().unwrap().to_string();

    let mut children = Vec::new();
    for i in 0..8 {
        let dir = dir.clone();
        let cap_s = cap_s.clone();
        children.push(std::thread::spawn(move || {
            let ns = run(forge().args([
                "--dir",
                &dir,
                "--cap",
                &cap_s,
                "session",
                "open",
                "--from=main",
            ]));
            let ns = ns.trim().to_string();
            run(forge().args([
                "--dir",
                &dir,
                "--cap",
                &cap_s,
                "write",
                "--ns",
                &ns,
                &format!("/p{i}.txt"),
                "--text",
                &format!("{i}"),
            ]));
            run(forge().args([
                "--dir", &dir, "--cap", &cap_s, "checkin", "--ns", &ns, "-m", "p",
            ]))
        }));
    }
    let mut updated = 0;
    for c in children {
        let out = c.join().unwrap();
        if out.contains("updated") {
            updated += 1;
        }
    }
    assert_eq!(updated, 8);
    let _ = Path::new(&dir);
}

#[test]
fn cli_merge_conflict_has_stable_exit_code_four() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let dir = d.path().to_str().unwrap();
    let root = d.path().join(".forge/keys/root.cap");
    let integ = d.path().join(".forge/keys/integrator.cap");
    let root = root.to_str().unwrap();
    let integ = integ.to_str().unwrap();

    let a = run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "session",
        "open",
        "--from=main",
    ]));
    let b = run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "session",
        "open",
        "--from=main",
    ]));
    let a = a.trim();
    let b = b.trim();
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "write",
        "--ns",
        a,
        "/same.txt",
        "--text",
        "ours",
    ]));
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "write",
        "--ns",
        b,
        "/same.txt",
        "--text",
        "theirs",
    ]));
    run(forge().args([
        "--dir", dir, "--cap", root, "checkin", "--ns", a, "-m", "ours",
    ]));
    run(forge().args([
        "--dir", dir, "--cap", root, "checkin", "--ns", b, "-m", "theirs",
    ]));

    let ref_a = format!("heads/agents/anon/{a}");
    let ref_b = format!("heads/agents/anon/{b}");
    run(forge().args([
        "--dir",
        dir,
        "--cap",
        integ,
        "merge",
        "--into=main",
        "--from",
        &ref_a,
    ]));
    let out = forge()
        .args([
            "--dir",
            dir,
            "--cap",
            integ,
            "merge",
            "--into=main",
            "--from",
            &ref_b,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("conflict"), "{stderr}");

    let conflict = stderr
        .lines()
        .find_map(|line| line.strip_prefix("conflict "))
        .expect("machine-readable conflict line");
    let shown = run(forge().args([
        "--dir",
        dir,
        "--cap",
        root,
        "show",
        &format!("oid:{conflict}"),
    ]));
    let ours = shown
        .lines()
        .find_map(|line| line.strip_prefix("ours "))
        .expect("conflict ours tree");

    // Keep accepting the legacy flag at the parser boundary, but never let a
    // raw Tree OID replace a merge result without conflict-bound provenance.
    let out = forge()
        .args([
            "--dir",
            dir,
            "--cap",
            integ,
            "merge",
            "--into=main",
            "--from",
            &ref_b,
            "--resolved",
            ours,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(RAW_MERGE_RESOLUTION_DISABLED),
        "unexpected stderr: {stderr}"
    );
}
