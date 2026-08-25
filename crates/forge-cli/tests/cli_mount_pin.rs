//! I19/I20 at the CLI boundary. Every refusal this change adds has to land on a
//! row CLI_ABI.md already defines -- exit 1, "denied/capability/input" -- and a
//! non-root read-write mount has to be publishable from the CLI, not only
//! through the API, or the fix is unreachable for the callers it is for.

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn init(dir: &Path) {
    assert!(forge()
        .arg("init")
        .arg(dir)
        .output()
        .unwrap()
        .status
        .success());
}

fn authed(dir: &Path) -> Command {
    let mut c = forge();
    c.arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"));
    c
}

/// stdout, stderr, exit code.
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

#[test]
fn a_non_root_read_write_mount_publishes_to_its_own_ref_and_fscks_clean() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"seed\n").unwrap();
    ok(
        &r,
        &["import", src.to_str().unwrap(), "--ref", "heads/seed"],
    );
    ok(&r, &["branch", "heads/seed", "base"]);
    ok(&r, &["branch", "heads/seed", "other"]);
    // The two refs must have DIVERGED, or a session pin and a mount pin are the
    // same commit and nothing distinguishes the two behaviours.
    let seed = ok(&r, &["session", "open", "--from", "base"]);
    let seed = seed.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &seed, "--rw", "/", "ref:base"]);
    ok(
        &r,
        &["write", "--ns", &seed, "/only-in-base.txt", "--text", "b"],
    );
    ok(&r, &["checkin", "--ns", &seed, "-m", "diverge"]);

    let ns = ok(&r, &["session", "open", "--from", "base"]);
    let ns = ns.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &ns, "--rw", "/", "ref:base"]);
    ok(&r, &["mount", "--ns", &ns, "--rw", "/other", "ref:other"]);
    ok(
        &r,
        &[
            "write",
            "--ns",
            &ns,
            "/other/w.txt",
            "--text",
            "lands in other",
        ],
    );

    // I19: the CLI can name the mount, and the ref that mount names is the one
    // that moves.
    let published = ok(
        &r,
        &["checkin", "--ns", &ns, "--mount", "/other", "-m", "x"],
    );
    assert!(
        published.starts_with("updated other "),
        "checkin must publish to the mount's own ref: {published}"
    );

    // And `base` did not acquire `other`'s entry.
    let ro = ok(&r, &["session", "open", "--from", "base"]);
    let ro = ro.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &ro, "/", "ref:base"]);
    let (_, _, code) = run(&r, &["read", "--ns", &ro, "/w.txt"]);
    assert_eq!(code, 1, "ref base must not hold ref other's content");

    // ...and `other` did not acquire `base`'s. This is the direction that broke:
    // the checkin used to fold onto the SESSION's base and CAS `other` from it.
    let ro = ok(&r, &["session", "open", "--from", "other"]);
    let ro = ro.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &ro, "/", "ref:other"]);
    ok(&r, &["read", "--ns", &ro, "/w.txt"]);
    let (_, _, code) = run(&r, &["read", "--ns", &ro, "/only-in-base.txt"]);
    assert_eq!(code, 1, "ref other must not hold ref base's content");

    let (_, stderr, code) = run(&r, &["fsck", "--full"]);
    assert_eq!(
        code, 0,
        "a repository with pinned mounts must fsck clean: {stderr}"
    );
}

#[test]
fn refusals_added_by_per_mount_pinning_are_exit_one() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"seed\n").unwrap();
    let imported = ok(
        &r,
        &["import", src.to_str().unwrap(), "--ref", "heads/seed"],
    );
    // "imported <oid> -> heads/seed"
    let oid = imported.split_whitespace().nth(1).unwrap().to_string();
    ok(&r, &["branch", "heads/seed", "base"]);
    ok(&r, &["branch", "heads/seed", "other"]);

    let ns = ok(&r, &["session", "open", "--from", "base"]);
    let ns = ns.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &ns, "--rw", "/", "ref:base"]);

    // I20: a read-write raw-oid mount is refused, not accepted-then-unpublishable.
    let spec = format!("oid:{oid}");
    let (_, stderr, code) = run(&r, &["mount", "--ns", &ns, "--rw", "/snap", &spec]);
    assert_eq!(code, 1, "a read-write oid mount must be exit 1: {stderr}");
    assert!(stderr.contains("read-write"), "{stderr}");
    // Read-only is fine.
    ok(&r, &["mount", "--ns", &ns, "/snap", &spec]);

    // I19: re-mounting a path over staged work is refused, not retargeted.
    ok(&r, &["mount", "--ns", &ns, "--rw", "/other", "ref:other"]);
    ok(
        &r,
        &["write", "--ns", &ns, "/other/s.txt", "--text", "staged"],
    );
    let (_, stderr, code) = run(&r, &["mount", "--ns", &ns, "--rw", "/other", "ref:base"]);
    assert_eq!(code, 1, "a retargeting re-mount must be exit 1: {stderr}");
    assert!(
        stderr.contains("/other") && stderr.contains("ref:other"),
        "{stderr}"
    );

    // The supported exit still works and leaves nothing behind.
    let published = ok(
        &r,
        &["checkin", "--ns", &ns, "--mount", "/other", "-m", "x"],
    );
    assert!(published.starts_with("updated other "), "{published}");
    let (_, _, code) = run(&r, &["abandon", "session", &ns]);
    assert_eq!(code, 0, "a session with nothing staged must be retirable");
}
