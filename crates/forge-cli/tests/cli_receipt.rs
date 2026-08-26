//! `forge receipt show` at the CLI boundary (I25, #71).
//!
//! Every refusal must land on a row CLI_ABI.md already defines, and the one
//! that matters is exit 2: a receipt naming an object that is not there is a
//! corrupt graph, not a missing file and not an input error.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authed(dir: &Path) -> Command {
    let mut c = forge();
    c.arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"));
    c
}

fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = authed(dir).args(args).output().expect("spawn forge");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status
            .code()
            .unwrap_or_else(|| panic!("killed: {out:?}")),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (stdout, stderr, code) = run(dir, args);
    assert_eq!(code, 0, "forge {args:?} exited {code}: {stderr}");
    stdout
}

fn field<'a>(rendered: &'a str, key: &str) -> &'a str {
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key} ")))
        .unwrap_or_else(|| panic!("no {key} line in:\n{rendered}"))
}

fn object_path(root: &Path, hex: &str) -> PathBuf {
    root.join(".forge/objects")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex)
}

#[test]
fn receipt_show_reports_the_frontier_and_refuses_a_receipt_that_lost_an_edge() {
    let d = tempdir().unwrap();
    let root = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&root)
        .output()
        .unwrap()
        .status
        .success());

    let seed_ns = ok(&root, &["session", "open", "--from", "main"]);
    ok(
        &root,
        &["write", "--ns", &seed_ns, "/seed.txt", "--text", "s"],
    );
    let seeded = ok(&root, &["checkin", "--ns", &seed_ns, "-m", "seed"]);
    let base_ref = seeded.split_whitespace().nth(1).unwrap().to_string();

    let ns = ok(&root, &["session", "open", "--from", &base_ref]);
    ok(&root, &["read", "--ns", &ns, "/seed.txt"]);
    ok(&root, &["write", "--ns", &ns, "/a.txt", "--text", "a"]);
    let published = ok(&root, &["checkin", "--ns", &ns, "-m", "work"]);
    let work_ref = published.split_whitespace().nth(1).unwrap().to_string();

    let rendered = ok(&root, &["receipt", "show", &work_ref]);
    assert!(rendered.contains("write /a.txt"), "{rendered}");
    assert!(rendered.contains("/seed.txt"), "{rendered}");
    assert!(rendered.starts_with("receipt "), "{rendered}");
    assert!(rendered.contains("\nresult "), "{rendered}");

    let receipt_oid = field(&rendered, "receipt").to_string();
    let tree = field(&rendered, "tree").to_string();

    // Naming the receipt object directly is the same receipt without a result.
    let direct = ok(&root, &["receipt", "show", &format!("oid:{receipt_oid}")]);
    assert!(!direct.contains("\nresult "), "{direct}");

    // A commit that carries no receipt at all is absence, not corruption (I10).
    let (_, stderr, code) = run(&root, &["receipt", "show", "main"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("no contribution receipt"), "{stderr}");

    // Remove the tree the receipt names. `show` still renders every field,
    // which is exactly the behaviour that made this invisible; `receipt`
    // refuses and names the edge.
    std::fs::remove_file(object_path(&root, &tree)).expect("tree object exists");
    let shown = ok(&root, &["show", &format!("oid:{receipt_oid}")]);
    assert!(
        shown.contains(&format!("tree {tree}")),
        "show is the unchecked surface: {shown}"
    );

    let (_, stderr, code) = run(&root, &["receipt", "show", &work_ref]);
    assert_eq!(
        code, 2,
        "a receipt naming an absent object is a corrupt graph: {stderr}"
    );
    assert!(
        stderr.contains(&tree),
        "the refusal names the edge: {stderr}"
    );
}

/// I14: reading a receipt is an authenticated command like every other.
#[test]
fn receipt_show_requires_a_capability() {
    let d = tempdir().unwrap();
    let root = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&root)
        .output()
        .unwrap()
        .status
        .success());
    let out = forge()
        .arg("--dir")
        .arg(&root)
        .args(["receipt", "show", "main"])
        .output()
        .expect("spawn forge");
    assert_eq!(out.status.code(), Some(1));
}
