//! #353: `forge checkin --mount <path>` acts on the mount it was NAMED, or it
//! refuses by name. It never silently substitutes the default `/` mount.
//!
//! `Forge::checkin` resolved its `--mount` argument through `longest_mount`,
//! the resolver for a PATH INSIDE a mount. Every session has a `/` mount and
//! `/` is a prefix of everything, so an unknown name always matched `/`:
//!
//!   * a misspelt `--mount` published `/` and reported `updated`, exit 0;
//!   * on a session with nothing staged it reported `noop`, exit 0 -- and
//!     CLI_ABI.md says "a `noop` is therefore a strong statement and callers
//!     may rely on it";
//!   * the I22 refusal read "checkin / has nothing to publish" for a request
//!     that named some other mount entirely.
//!
//! All three contradict CLI_ABI.md's "Checkin folds exactly the named mount",
//! and they are the CLI analogue of the daemon defect fixed in #332, after
//! which the daemon refuses an unknown field by name and lists the accepted
//! set. CLI_ABI.md specifies the daemon as a strict projection of the CLI, so
//! the CLI cannot be the looser of the two (I19, I22).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn run(dir: &Path, args: &[&str]) -> Output {
    let mut c = forge();
    c.arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"));
    c.args(args);
    c.output().expect("spawn forge")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "forge {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Every ref and the commit it holds, so a test can assert that a refused
/// command moved nothing rather than merely that it printed something.
fn heads(dir: &Path) -> BTreeMap<String, String> {
    ok(dir, &["refs"])
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace().rev();
            let oid = f.next()?.to_string();
            let name = f.next()?.to_string();
            Some((name, oid))
        })
        .collect()
}

#[test]
fn an_unknown_mount_publishes_nothing_and_is_refused_by_name() {
    let d = tempdir().unwrap();
    let repo = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    let ns = ok(&repo, &["session", "open", "--from", "main"]);
    // Staged work under the DEFAULT mount, so a fallback to `/` would publish
    // something and be visible as a moved ref.
    ok(&repo, &["write", "--ns", &ns, "/a.txt", "--text", "hi"]);

    let before = heads(&repo);
    let out = run(
        &repo,
        &[
            "checkin",
            "--ns",
            &ns,
            "--mount",
            "/this-mount-does-not-exist",
            "-m",
            "typo",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert_eq!(
        out.status.code(),
        Some(1),
        "a checkin naming a mount the session does not have is a not-found input error \
         (CLI_ABI.md exit 1): stdout={stdout} stderr={stderr}"
    );
    assert_eq!(
        heads(&repo),
        before,
        "checkin folds exactly the named mount; naming one that does not exist must move NO ref, \
         and it published the default / mount instead: stdout={stdout}"
    );
    assert!(
        stderr.contains("/this-mount-does-not-exist"),
        "the refusal must name the mount the CALLER named: {stderr}"
    );

    // The session is untouched, so naming the real mount still publishes it.
    let published = ok(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/", "-m", "real"],
    );
    assert!(
        published.starts_with("updated ") || published.starts_with("forked "),
        "the staged write must still be there to publish: {published}"
    );
}

#[test]
fn an_unknown_mount_never_answers_noop() {
    let d = tempdir().unwrap();
    let repo = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    let ns = ok(&repo, &["session", "open", "--from", "main"]);

    let out = run(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/nope", "-m", "typo"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !stdout.contains("noop"),
        "CLI_ABI.md: a noop means the session holds no staged work anywhere, and callers may rely \
         on it. It may not also mean the mount you asked about does not exist: {stdout}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The clean session itself still noops when the default mount is named for
    // real, so the refusal above is about the NAME and not about the session.
    let noop = ok(&repo, &["checkin", "--ns", &ns, "-m", "nothing"]);
    assert!(noop.starts_with("noop "), "{noop}");
}

#[test]
fn the_i22_refusal_names_the_mount_the_caller_named() {
    let d = tempdir().unwrap();
    let repo = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    ok(&repo, &["branch", "main", "shared"]);
    let ns = ok(&repo, &["session", "open", "--from", "main"]);
    ok(
        &repo,
        &["mount", "--ns", &ns, "/rw-ok", "ref:shared", "--rw"],
    );
    ok(
        &repo,
        &["write", "--ns", &ns, "/rw-ok/a.txt", "--text", "hi"],
    );

    // One character off the real mount name.
    let out = run(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/rw-okk", "-m", "typo"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(1), "stderr={stderr}");
    assert!(
        stderr.contains("/rw-okk"),
        "a refusal must be about the mount the request named: {stderr}"
    );
    assert!(
        !stderr.contains("checkin / has nothing to publish"),
        "the I22 diagnostic reported on `/`, a mount this request never mentioned: {stderr}"
    );

    // The correctly spelled name still publishes, so nothing above is a blanket
    // refusal of `--mount`.
    let published = ok(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/rw-ok", "-m", "real"],
    );
    assert!(published.starts_with("updated shared "), "{published}");
}

#[test]
fn a_path_inside_a_mount_is_not_a_mount() {
    let d = tempdir().unwrap();
    let repo = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    ok(&repo, &["branch", "main", "shared"]);
    let ns = ok(&repo, &["session", "open", "--from", "main"]);
    ok(
        &repo,
        &["mount", "--ns", &ns, "/rw-ok", "ref:shared", "--rw"],
    );
    ok(
        &repo,
        &["write", "--ns", &ns, "/rw-ok/sub/a.txt", "--text", "hi"],
    );

    let before = heads(&repo);
    let out = run(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/rw-ok/sub", "-m", "sub"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "`--mount` names a mount, not a path under one: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        heads(&repo),
        before,
        "publishing /rw-ok for a request that named /rw-ok/sub folds a different mount than the \
         one asked for"
    );

    // A trailing slash is spelling, not a different mount.
    let published = ok(
        &repo,
        &["checkin", "--ns", &ns, "--mount", "/rw-ok/", "-m", "real"],
    );
    assert!(published.starts_with("updated shared "), "{published}");
}
