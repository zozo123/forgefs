//! I24: the intermediate state of a move is never observable (#39).
//!
//! `forge mv` stages the destination and the source tombstone in ONE catalog
//! transaction. This file kills the process at the exact point where the two
//! halves are apart and shows that a COLD reopen -- a different process, so no
//! `Store` LRU can answer -- sees the move either wholly done or wholly not
//! done, and never the file at both paths or at neither.
//!
//! ## What this evidences, and what it does not
//!
//! `libc::_exit` and `SIGKILL` both end the process without running Rust or
//! SQLite destructors, so an uncommitted transaction is lost exactly as it
//! would be on an abrupt process death. That is the property under test:
//! ATOMICITY of the catalog transaction across process loss.
//!
//! Neither is evidence about POWER loss. The kernel keeps the page cache and
//! the WAL file across both, so bytes this process wrote are still readable by
//! the next one whether or not any barrier ran. Durability across power loss is
//! I4's property, it is established by the barrier machinery, and it is
//! measured elsewhere: `barrier_fault_injection.rs`, `cli_sigkill.rs` and
//! `docs/RECOVERY.md`. Nothing in this file may be cited for it.
//!
//! ## Why the test is not vacuous
//!
//! `FORGEFS_TEST_RENAME_UNSAFE_SPLIT` (debug builds only) stages the same move
//! the only way copy+delete can: two independent autocommit transactions. The
//! crash point is the same, the timing is the same, the assertion is the same
//! -- and the file is durably present at BOTH paths afterwards. That is the
//! outcome `atomically_staged_move_is_all_or_nothing` forbids, produced on
//! demand, so the assertion is known to be one a wrong implementation fails.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::{tempdir, TempDir};

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

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = support::output(authed(dir).args(args));
    assert!(
        out.status.success(),
        "forge {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository holding `/x` on a private ref, plus a live session mounted
/// read-write on it. The move under test is `/x` -> `/y`.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    ns: String,
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let root = dir.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&root)
        .output()
        .unwrap()
        .status
        .success());
    let seed_ns = ok(&root, &["session", "open", "--from", "main"]);
    ok(&root, &["write", "--ns", &seed_ns, "/x", "--text", "v1"]);
    let published = ok(&root, &["checkin", "--ns", &seed_ns, "-m", "seed"]);
    let target = published
        .split_whitespace()
        .nth(1)
        .expect("checkin prints `updated <ref> <oid>`")
        .to_string();
    let ns = ok(&root, &["session", "open", "--from", &target]);
    Fixture {
        _dir: dir,
        root,
        ns,
    }
}

impl Fixture {
    /// What a FRESH process sees at the mount root. A cold open on purpose: a
    /// live `Store` keeps hot LRU caches, so a same-process read could answer
    /// out of memory rather than out of the catalog the crash left behind.
    fn cold_listing(&self) -> Vec<String> {
        ok(&self.root, &["ls", "--ns", &self.ns, "/"])
            .lines()
            .map(|l| l.split_whitespace().last().unwrap_or_default().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }

    fn mv(&self) -> Command {
        let mut c = authed(&self.root);
        c.args(["mv", "--ns", &self.ns, "/x", "/y"]);
        c
    }
}

const BEFORE: &[&str] = &["x"];
const AFTER: &[&str] = &["y"];

/// Crash inside the staged move, between the destination row and the source
/// tombstone. The transaction never commits, so the cold reopen is the state
/// before the move: `/x` alone, never `/x` and `/y`, never neither.
#[test]
fn atomically_staged_move_is_all_or_nothing() {
    let f = fixture();
    let out = support::output(
        f.mv()
            .env("FORGEFS_TEST_RENAME_CRASH_AFTER", "staged-destination"),
    );
    assert_eq!(
        out.status.code(),
        Some(86),
        "the crash point must end the process without unwinding: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        f.cold_listing(),
        BEFORE,
        "an uncommitted move must leave no trace; a duplicate here is the \
         copy+delete failure #39 exists to remove"
    );
    // And the move still works afterwards: the crash left no half-state that
    // blocks the retry, which is the other half of "all or nothing".
    let out = support::output(&mut f.mv());
    assert!(out.status.success(), "retry after the crash must succeed");
    assert_eq!(f.cold_listing(), AFTER);
}

/// The same crash, staged the way copy+delete has to stage it. This is the
/// control: it fails the assertion above, so that assertion is testing
/// something. Debug-only; `FORGEFS_TEST_RENAME_UNSAFE_SPLIT` does not exist in
/// a release build.
#[test]
#[cfg(debug_assertions)]
fn the_split_spelling_leaves_the_duplicate_this_test_forbids() {
    let f = fixture();
    let out = support::output(
        f.mv()
            .env("FORGEFS_TEST_RENAME_CRASH_AFTER", "staged-destination")
            .env("FORGEFS_TEST_RENAME_UNSAFE_SPLIT", "1"),
    );
    assert_eq!(out.status.code(), Some(86));
    assert_eq!(
        f.cold_listing(),
        vec!["x".to_string(), "y".to_string()],
        "two transactions must be observable apart -- if this ever stops \
         duplicating, the atomicity test above has stopped proving anything"
    );
}

/// A real signal, not an injected exit: SIGKILL the mv at a sweep of offsets
/// covering "before it opened the catalog" through "after it finished". Every
/// cold reopen must be one of the two legal states. This adds nothing about
/// power loss (see the module docs); it adds that the property survives a kill
/// the process cannot see coming, at an offset no hook chose.
#[test]
#[cfg(unix)]
fn sigkill_at_any_offset_leaves_one_of_the_two_legal_states() {
    let mut reached_the_move = false;
    for delay_ms in [0u64, 1, 2, 4, 16, 64, 250] {
        let f = fixture();
        let mut child = f
            .mv()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn forge mv");
        std::thread::sleep(Duration::from_millis(delay_ms));
        // Failure means it already exited; that is a legal offset too.
        let _ = child.kill();
        let _ = child.wait();

        let seen = f.cold_listing();
        assert!(
            seen == BEFORE || seen == AFTER,
            "SIGKILL at +{delay_ms}ms left {seen:?}; the only observable states \
             are {BEFORE:?} and {AFTER:?}"
        );
        reached_the_move |= seen == AFTER;
    }
    assert!(
        reached_the_move,
        "no offset in the sweep let the move commit, so the sweep proved \
         nothing about the mutation; widen the largest delay"
    );
}
