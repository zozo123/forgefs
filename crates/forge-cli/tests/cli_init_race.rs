//! Process-level proof that repository initialization has one atomic publisher.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const INITIALIZERS: usize = 8;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
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

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn staging_dirs(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".forge.init-"))
        })
        .collect()
}

#[test]
fn cli_concurrent_init_has_exactly_one_publisher_and_no_staging_leaks() {
    let d = tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(INITIALIZERS));
    let mut launchers = Vec::with_capacity(INITIALIZERS);

    for _ in 0..INITIALIZERS {
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            forge()
                .arg("init")
                .current_dir(dir)
                .output()
                .expect("spawn racing forge init")
        }));
    }

    let results = launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join init launcher"))
        .collect::<Vec<_>>();

    let winners = results.iter().filter(|out| out.status.success()).count();
    assert_eq!(
        winners, 1,
        "atomic no-replace publication must have one winner; results={:?}",
        results
            .iter()
            .map(|out| (out.status.code(), String::from_utf8_lossy(&out.stderr)))
            .collect::<Vec<_>>()
    );
    for out in results.iter().filter(|out| !out.status.success()) {
        assert_ne!(out.status.code(), Some(0));
        assert!(
            out.stdout.is_empty(),
            "a losing initializer must not report a published root: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    let forge_root = d.path().join(".forge");
    assert_eq!(fs::read(forge_root.join("VERSION")).unwrap(), b"1\n");
    let root_cap = forge_root.join("keys/root.cap");
    let root_secret = fs::read(forge_root.join("keys/root.secret")).unwrap();
    let seal_key = fs::read(forge_root.join("keys/seal.ed25519")).unwrap();
    assert!(root_cap.is_file());

    // Every losing process built under a unique sibling staging name. Once all
    // children have exited, no failed publisher may leave a shadow repository.
    assert!(
        staging_dirs(d.path()).is_empty(),
        "losing initializers leaked staging directories: {:?}",
        staging_dirs(d.path())
    );

    // A fresh process must see one coherent catalog/key/object cell.
    let mut refs = authenticated(d.path(), &root_cap);
    refs.arg("refs");
    let refs = run(&mut refs);
    assert!(refs.contains("main"), "missing main after init race: {refs}");

    let mut fsck = authenticated(d.path(), &root_cap);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);

    // A later initializer must fail without replacing the winner's authority.
    let retry = output(forge().arg("init").current_dir(d.path()));
    assert!(!retry.status.success(), "completed cell was reinitialized");
    assert_eq!(
        fs::read(forge_root.join("keys/root.secret")).unwrap(),
        root_secret,
        "failed retry replaced the HMAC root"
    );
    assert_eq!(
        fs::read(forge_root.join("keys/seal.ed25519")).unwrap(),
        seal_key,
        "failed retry replaced the signing key"
    );

    let mut final_fsck = authenticated(d.path(), &root_cap);
    final_fsck.arg("fsck").arg("--full");
    run(&mut final_fsck);
}
