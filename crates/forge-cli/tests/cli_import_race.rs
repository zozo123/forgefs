//! Process-level proof that concurrent imports report the ref that actually owns their commit.

use forge_api::Forge;
use forge_types::RefRow;
use std::fs;
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
    run(forge().arg("init").current_dir(dir));
    dir.join(".forge/keys/root.cap")
}

fn make_source(parent: &Path, name: &str, marker: u8) -> PathBuf {
    let source = parent.join(name);
    fs::create_dir(&source).unwrap();
    fs::write(source.join("marker.txt"), [marker]).unwrap();
    fs::write(source.join(format!("only-{name}.txt")), name.as_bytes()).unwrap();
    // Enough real object work that the test remains representative even though
    // a debug-only post-snapshot barrier makes the CAS race deterministic.
    for index in 0..4 {
        fs::write(
            source.join(format!("bulk-{index}.bin")),
            vec![marker; 2 * 1024 * 1024],
        )
        .unwrap();
    }
    source
}

fn import(dir: &Path, cap: &Path, source: &Path, barrier: Option<&Path>) -> Output {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("import")
        .arg(source)
        .arg("--ref")
        .arg("heads/import");
    if let Some(barrier) = barrier {
        cmd.env("FORGEFS_TEST_IMPORT_SNAPSHOT_BARRIER", barrier);
    }
    output(&mut cmd)
}

fn find_ref<'a>(refs: &'a [RefRow], name: &str) -> &'a RefRow {
    refs.iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("missing ref {name}: {refs:?}"))
}

fn marker_at(forge: &Forge, cap: &forge_cap::Cap, r#ref: &str) -> u8 {
    let ns = forge.session_open(cap, r#ref).unwrap();
    forge.read(cap, &ns, "/marker.txt").unwrap()[0]
}

fn fork_name(stdout: &str) -> Option<String> {
    let mut fields = stdout.split_whitespace();
    if fields.next()? != "forked" {
        return None;
    }
    let requested = fields.next()?;
    if requested != "heads/import" || fields.next()? != "->" {
        return None;
    }
    fields.next().map(str::to_string)
}

#[test]
fn concurrent_cli_imports_preserve_and_truthfully_report_the_loser() {
    let d = tempdir().unwrap();
    let root = init(d.path());

    let baseline = d.path().join("baseline");
    fs::create_dir(&baseline).unwrap();
    fs::write(baseline.join("marker.txt"), b"0").unwrap();
    let baseline_result = import(d.path(), &root, &baseline, None);
    assert!(baseline_result.status.success(), "{baseline_result:?}");

    let before = Forge::open(d.path()).unwrap();
    let before_cap = before.root_cap().unwrap();
    let baseline_oid = before.peel_commit("heads/import").unwrap().0;
    drop(before_cap);
    drop(before);

    let left = make_source(d.path(), "left", b'L');
    let right = make_source(d.path(), "right", b'R');
    let barrier_dir = d.path().join("import-snapshot-barrier");
    fs::create_dir(&barrier_dir).unwrap();

    let launch = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for source in [left, right] {
        let launch = Arc::clone(&launch);
        let repo = d.path().to_path_buf();
        let cap = root.clone();
        let barrier = barrier_dir.clone();
        workers.push(std::thread::spawn(move || {
            launch.wait();
            import(&repo, &cap, &source, Some(&barrier))
        }));
    }

    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("join import worker"))
        .collect::<Vec<_>>();
    assert!(
        results.iter().all(|result| result.status.success()),
        "both imports preserve work and must exit successfully: {:?}",
        results
            .iter()
            .map(|result| (
                result.status.code(),
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            ))
            .collect::<Vec<_>>()
    );

    let stdout = results
        .iter()
        .map(|result| String::from_utf8(result.stdout.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        stdout
            .iter()
            .filter(|line| line.starts_with("imported ") && line.contains(" -> heads/import"))
            .count(),
        1,
        "exactly one process may report the requested ref as updated: {stdout:?}"
    );
    let forks = stdout
        .iter()
        .filter_map(|line| fork_name(line))
        .collect::<Vec<_>>();
    assert_eq!(
        forks.len(),
        1,
        "the CAS loser must name its fork: {stdout:?}"
    );
    let fork = &forks[0];

    let reopened = Forge::open(d.path()).unwrap();
    let cap = reopened.root_cap().unwrap();
    let refs = reopened.refs(&cap).unwrap();
    let target = find_ref(&refs, "heads/import");
    let loser = find_ref(&refs, fork);
    assert_ne!(target.oid, loser.oid, "winner and loser commits collapsed");

    let (_, winner_commit) = reopened.peel_commit("heads/import").unwrap();
    let (_, loser_commit) = reopened.peel_commit(fork).unwrap();
    assert_eq!(winner_commit.parents, vec![baseline_oid]);
    assert_eq!(loser_commit.parents, vec![baseline_oid]);

    let winner_marker = marker_at(&reopened, &cap, "heads/import");
    let loser_marker = marker_at(&reopened, &cap, fork);
    assert!(matches!(winner_marker, b'L' | b'R'));
    assert!(matches!(loser_marker, b'L' | b'R'));
    assert_ne!(
        winner_marker, loser_marker,
        "snapshots mixed or work was lost"
    );

    let winner_only = if winner_marker == b'L' {
        "/only-left.txt"
    } else {
        "/only-right.txt"
    };
    let winner_other = if winner_marker == b'L' {
        "/only-right.txt"
    } else {
        "/only-left.txt"
    };
    let loser_only = if loser_marker == b'L' {
        "/only-left.txt"
    } else {
        "/only-right.txt"
    };
    let loser_other = if loser_marker == b'L' {
        "/only-right.txt"
    } else {
        "/only-left.txt"
    };
    let winner_ns = reopened.session_open(&cap, "heads/import").unwrap();
    let loser_ns = reopened.session_open(&cap, fork).unwrap();
    assert!(reopened.read(&cap, &winner_ns, winner_only).is_ok());
    assert!(reopened.read(&cap, &winner_ns, winner_other).is_err());
    assert!(reopened.read(&cap, &loser_ns, loser_only).is_ok());
    assert!(reopened.read(&cap, &loser_ns, loser_other).is_err());

    let report = reopened.fsck(&cap, true).unwrap();
    assert!(
        report.ok,
        "concurrent import left corruption: {:?}",
        report.findings
    );
}
