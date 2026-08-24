//! Process-level proof that concurrent writers CAS one shared ref without lost updates.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const WRITERS: usize = 8;

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

fn fork_name(stdout: &str, requested: &str) -> Option<String> {
    let mut fields = stdout.split_whitespace();
    if fields.next()? != "forked" || fields.next()? != requested || fields.next()? != "->" {
        return None;
    }
    fields.next().map(str::to_string)
}

#[test]
fn cli_shared_ref_stampede_has_one_winner_and_seven_forks() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap().to_string();

    let mut branch = authenticated(d.path(), &root);
    branch.arg("branch").arg("main").arg("heads/hot");
    run(&mut branch);

    let mut sessions = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        let mut open = authenticated(d.path(), &root);
        open.arg("session").arg("open").arg("--from=heads/hot");
        let ns = run(&mut open).trim().to_string();

        // Replace this session's private root mount with the same shared live
        // ref for every writer. The pinned base remains the heads/hot commit
        // observed at session_open, so checkin races one real CAS name.
        let mut mount = authenticated(d.path(), &root);
        mount
            .arg("mount")
            .arg("--ns")
            .arg(&ns)
            .arg("/")
            .arg("heads/hot")
            .arg("--rw");
        run(&mut mount);

        let mut write = authenticated(d.path(), &root);
        write
            .arg("write")
            .arg("--ns")
            .arg(&ns)
            .arg(format!("/writer-{i}.txt"))
            .arg("--text")
            .arg(format!("writer-{i}"));
        run(&mut write);
        sessions.push((i, ns));
    }

    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut launchers = Vec::with_capacity(WRITERS);
    for (writer, ns) in sessions {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            let output = authenticated(&dir, &cap)
                .arg("checkin")
                .arg("--ns")
                .arg(&ns)
                .arg("-m")
                .arg("shared stampede")
                .output()
                .expect("spawn concurrent forge checkin");
            (writer, ns, output)
        }));
    }

    let mut updated = 0;
    let mut forked = 0;
    let mut completed = Vec::with_capacity(WRITERS);
    for launcher in launchers {
        let (writer, ns, out) = launcher.join().unwrap();
        assert!(
            out.status.success(),
            "checkin failed status={:?}\nstdout={}\nstderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("updated heads/hot") {
            updated += 1;
        } else if fork_name(&stdout, "heads/hot").is_some() {
            forked += 1;
        } else {
            panic!("unexpected checkin result: {stdout}");
        }
        completed.push((writer, ns, stdout.into_owned()));
    }

    assert_eq!(updated, 1, "shared CAS must have exactly one winner");
    assert_eq!(
        forked,
        WRITERS - 1,
        "every loser must be preserved as a fork"
    );

    // I18: a successful checkin transition clears the overlay only after its
    // contribution is durably reachable. Prove every losing process retained
    // its exact bytes both through the retargeted session and through a fresh
    // session opened from the reported fork ref.
    for (writer, ns, stdout) in &completed {
        let Some(fork) = fork_name(stdout, "heads/hot") else {
            continue;
        };
        let path = format!("/writer-{writer}.txt");
        let expected = format!("writer-{writer}");

        let mut read = authenticated(d.path(), &root);
        read.arg("read").arg("--ns").arg(ns).arg(&path);
        assert_eq!(run(&mut read), expected, "retargeted session lost {path}");

        let mut open = authenticated(d.path(), &root);
        open.arg("session")
            .arg("open")
            .arg(format!("--from={fork}"));
        let fork_ns = run(&mut open).trim().to_string();

        let mut read_fork = authenticated(d.path(), &root);
        read_fork.arg("read").arg("--ns").arg(&fork_ns).arg(&path);
        assert_eq!(
            run(&mut read_fork),
            expected,
            "durable fork {fork} lost {path}"
        );
    }

    let mut refs = authenticated(d.path(), &root);
    refs.arg("refs");
    let refs = run(&mut refs);
    assert!(refs.contains("heads/hot"), "shared ref disappeared: {refs}");

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
