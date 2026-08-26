//! #359: a repository larger than this build will WALK is not a corrupt one.
//!
//! `GraphWorkQueue` bounds how many distinct objects one traversal holds in
//! memory. A repository that grew past that bound through ordinary `import`
//! and `checkin` is intact -- every object file rehashes to its own name, every
//! typed edge resolves -- so the bound must never be spent as exit 2, the code
//! CLI_ABI.md reserves for damaged bytes. Before this change `fsck --full`,
//! `gc` and `verify` all answered `corrupt: object graph exceeded 1000000
//! objects` and exited 2, and those are precisely the commands an operator
//! reaches for when a repository has grown large.
//!
//! A million real objects is not a test. `FORGEFS_MAX_GRAPH_OBJECTS` makes the
//! ceiling settable, so the CLASSIFICATION is exercised here at a ceiling of
//! three while the DEFAULT ceiling stays exactly what it was. What this file
//! therefore proves is the exit code, the wording and the fallback; what it
//! does not prove, and nothing here claims, is the memory behaviour of a real
//! million-object walk.

use std::path::Path;
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

/// stdout, stderr, exit code, with an optional explicit walk ceiling.
fn run(dir: &Path, limit: Option<&str>, args: &[&str]) -> (String, String, i32) {
    let mut cmd = authed(dir);
    match limit {
        Some(value) => cmd.env("FORGEFS_MAX_GRAPH_OBJECTS", value),
        None => cmd.env_remove("FORGEFS_MAX_GRAPH_OBJECTS"),
    };
    let out = cmd.args(args).output().expect("spawn forge");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status
            .code()
            .unwrap_or_else(|| panic!("forge died by signal: {out:?}")),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (stdout, stderr, code) = run(dir, None, args);
    assert_eq!(code, 0, "forge {args:?} exited {code}: {stderr}");
    stdout
}

/// A tiny but genuinely multi-object repository: a sealed commit over a tree
/// with a subdirectory, so every walked graph has more than three objects in it.
fn repository() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&r)
        .output()
        .unwrap()
        .status
        .success());
    ok(&r, &["branch", "main", "work"]);
    let ns = ok(&r, &["session", "open", "--from", "work"]);
    let ns = ns.lines().last().unwrap().to_string();
    ok(&r, &["mount", "--ns", &ns, "--rw", "/", "ref:work"]);
    ok(&r, &["write", "--ns", &ns, "/a.txt", "--text", "hello"]);
    ok(&r, &["write", "--ns", &ns, "/d/b.txt", "--text", "world"]);
    ok(&r, &["checkin", "--ns", &ns, "-m", "one"]);
    ok(&r, &["seal", "work", "--tag", "v1"]);
    (d, r)
}

/// The commands an operator runs on a large repository, at a ceiling the
/// repository exceeds. Exit 1, and nothing anywhere says the repository is
/// corrupt.
#[test]
fn a_graph_larger_than_the_walk_ceiling_is_refused_not_called_corrupt() {
    let (_d, r) = repository();

    for args in [
        vec!["fsck"],
        vec!["fsck", "--full"],
        vec!["gc", "--dry-run"],
        vec!["verify", "v1"],
    ] {
        let (stdout, stderr, code) = run(&r, Some("3"), &args);
        assert_eq!(
            code, 1,
            "forge {args:?} must refuse the walk (exit 1), not report corruption: \
             code={code} stdout={stdout} stderr={stderr}"
        );
        let said = format!("{stdout}\n{stderr}");
        assert!(
            said.contains("not corrupt"),
            "forge {args:?} must say the bytes are intact: {said}"
        );
        assert!(
            said.contains("FORGEFS_MAX_GRAPH_OBJECTS"),
            "forge {args:?} must name the remedy: {said}"
        );
        assert!(
            !said.contains("corrupt: "),
            "forge {args:?} must not render a corruption error: {said}"
        );
        assert!(
            !said.contains("GRAPH_LIMIT"),
            "the ceiling is a refusal, not an fsck finding about the objects: {said}"
        );
    }
}

/// The same repository, same commands, at the default ceiling: everything
/// passes. Without this the test above would also pass on a build that simply
/// refused every walk.
#[test]
fn the_default_ceiling_walks_an_ordinary_repository() {
    let (_d, r) = repository();

    for args in [
        vec!["fsck"],
        vec!["fsck", "--full"],
        vec!["gc", "--dry-run"],
        vec!["verify", "v1"],
    ] {
        let (stdout, stderr, code) = run(&r, None, &args);
        assert_eq!(code, 0, "forge {args:?}: {stdout} {stderr}");
    }
}

/// The override may only ever RAISE a walk that already refuses, so a value
/// this build cannot use falls back to the default rather than failing a
/// command that was fine. A typo in an environment variable must not be able to
/// break `fsck`.
#[test]
fn an_unusable_ceiling_falls_back_to_the_default_instead_of_failing() {
    let (_d, r) = repository();

    for value in ["", "0", "lots", "-4", "  "] {
        let (stdout, stderr, code) = run(&r, Some(value), &["fsck", "--full"]);
        assert_eq!(
            code, 0,
            "FORGEFS_MAX_GRAPH_OBJECTS={value:?} must not break an ordinary fsck: \
             {stdout} {stderr}"
        );
    }
}
