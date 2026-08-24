//! Process-level proof that content identity is global but authority is cell-local.

use std::fs;
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
    fs::create_dir_all(dir).unwrap();
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    dir.join(".forge/keys/root.cap")
}

fn open_session(dir: &Path, cap: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    run(&mut cmd).trim().to_string()
}

fn write_same_blob(dir: &Path, cap: &str, ns: &str) -> String {
    let mut write = authenticated(dir, cap);
    write
        .arg("write")
        .arg("--ns")
        .arg(ns)
        .arg("/same.txt")
        .arg("--text")
        .arg("same immutable bytes");
    run(&mut write).trim().to_string()
}

fn assert_foreign_cap_denied(dir: &Path, foreign_cap: &str) {
    let mut refs = authenticated(dir, foreign_cap);
    refs.arg("refs");
    let out = output(&mut refs);
    assert_eq!(
        out.status.code(),
        Some(1),
        "foreign capability must fail at the CLI authority boundary"
    );
    assert!(
        out.stdout.is_empty(),
        "foreign capability disclosed refs: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let mut session = authenticated(dir, foreign_cap);
    session.arg("session").arg("open").arg("--from=main");
    let out = output(&mut session);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "foreign capability opened a namespace: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn fsck(dir: &Path, cap: &str) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

#[test]
fn cli_cross_cell_capability_isolation_survives_identical_object_ids() {
    let d = tempdir().unwrap();
    let a = d.path().join("a");
    let b = d.path().join("b");
    let a_cap_path = init(&a);
    let b_cap_path = init(&b);
    let a_token = fs::read_to_string(&a_cap_path).unwrap();
    let b_token = fs::read_to_string(&b_cap_path).unwrap();
    let a_token = a_token.trim();
    let b_token = b_token.trim();

    assert_ne!(
        a_token, b_token,
        "independent cells reused a root capability"
    );
    assert_ne!(
        fs::read(a.join(".forge/keys/root.secret")).unwrap(),
        fs::read(b.join(".forge/keys/root.secret")).unwrap(),
        "independent cells reused the HMAC root"
    );

    let a_ns = open_session(&a, a_token);
    let b_ns = open_session(&b, b_token);
    let a_oid = write_same_blob(&a, a_token, &a_ns);
    let b_oid = write_same_blob(&b, b_token, &b_ns);
    assert_eq!(
        a_oid, b_oid,
        "content-addressed identity should be independent of the owning cell"
    );

    // Same immutable ObjectId does not imply shared authority: the capability
    // authenticates against the concrete cell's independent HMAC root first.
    assert_foreign_cap_denied(&a, b_token);
    assert_foreign_cap_denied(&b, a_token);

    // Path-valued --cap is equally isolated; pointing B at A's root.cap must
    // not become an ambient/root escape hatch.
    let a_cap_path = a_cap_path.to_str().unwrap();
    let b_cap_path = b_cap_path.to_str().unwrap();
    assert_foreign_cap_denied(&a, b_cap_path);
    assert_foreign_cap_denied(&b, a_cap_path);

    // The correct cell-local credentials still work after all failed foreign
    // attempts, and neither repository was mutated by them.
    let mut a_refs = authenticated(&a, a_token);
    a_refs.arg("refs");
    assert!(run(&mut a_refs).contains("main"));
    let mut b_refs = authenticated(&b, b_token);
    b_refs.arg("refs");
    assert!(run(&mut b_refs).contains("main"));
    fsck(&a, a_token);
    fsck(&b, b_token);
}
