//! Process-level e2e: real `forge` binaries, required --cap, parallel checkin.

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

#[test]
fn cli_requires_cap() {
    let d = tempdir().unwrap();
    run(forge().args(["init"]).current_dir(d.path()));
    let out = forge()
        .args(["--dir", d.path().to_str().unwrap(), "refs"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no ambient root"), "{err}");
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
