//! I5 for `seal`: the tag a seal publishes names the ref it read, or the seal
//! refuses.
//!
//! `seal` reads a ref, builds a signed Snapshot naming that commit, and only
//! then publishes `tags/<tag>`. Every other ref-moving verb in ForgeFS closes
//! that window with a compare-and-swap (I5: refs move only expected -> new).
//! Seal did not, so between the read and the publish another process could move
//! the head and the tag would name a commit the ref no longer held -- silently,
//! exit 0, and with `verify` still passing, because the sealed snapshot is
//! internally consistent. A seal is the provenance claim "this ref was this
//! commit at this moment"; a claim that can be false with no error is the whole
//! defect.
//!
//! `FORGEFS_TEST_SEAL_CAS_BARRIER` is the seal-side analogue of
//! `FORGEFS_TEST_CHECKIN_CAS_BARRIER`: a debug-only rendezvous placed after the
//! ref read and before the seal transaction, so the window is raced
//! deterministically instead of argued from source.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
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
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    dir.join(".forge/keys/root.cap")
}

/// The oid a ref holds right now, read back through the CLI so the assertion
/// keys on published state and never on a value this test remembered.
fn ref_oid(dir: &Path, cap: &Path, name: &str) -> Option<String> {
    let listing = run(authenticated(dir, cap).arg("refs"));
    listing
        .lines()
        .find(|line| line.split_whitespace().any(|field| field == name))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
}

/// The COMMIT a published tag names. `branch` peels a Snapshot to its commit,
/// so this reads the seal's own claim rather than the snapshot oid.
fn sealed_commit(dir: &Path, cap: &Path, tag: &str, probe: &str) -> String {
    let line = run(authenticated(dir, cap)
        .arg("branch")
        .arg(format!("tags/{tag}"))
        .arg(probe));
    line.split_whitespace()
        .last()
        .unwrap_or_else(|| panic!("branch printed no oid: {line}"))
        .to_string()
}

fn write_and_checkin(dir: &Path, cap: &Path, ns: &str, path: &str, text: &str) {
    run(authenticated(dir, cap)
        .arg("write")
        .arg("--ns")
        .arg(ns)
        .arg(path)
        .arg("--text")
        .arg(text));
    let result = run(authenticated(dir, cap)
        .arg("checkin")
        .arg("--ns")
        .arg(ns)
        .arg("-m")
        .arg(text));
    assert!(
        result.starts_with("updated"),
        "checkin of {path} did not update the head: {result}"
    );
}

/// Wait until the sealing process has parked on the barrier. Its marker file is
/// created strictly after `seal` has read the ref, so once it exists the read
/// this test is racing has already happened.
fn wait_for_parked_sealer(barrier: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let parked = std::fs::read_dir(barrier)
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        if parked >= 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "seal never reached the {} barrier",
            barrier.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cli_seal_refuses_when_the_ref_moves_inside_the_seal_window() {
    let d = tempdir().unwrap();
    let root = init(d.path());

    let ns = run(authenticated(d.path(), &root)
        .arg("session")
        .arg("open")
        .arg("--from=main"))
    .trim()
    .to_string();
    let head = format!("heads/agents/anon/{ns}");

    write_and_checkin(d.path(), &root, &ns, "/a.txt", "first");
    let observed = ref_oid(d.path(), &root, &head).expect("head exists after the first checkin");

    // Park a sealer inside its own window, after it has read `head`.
    let barrier = d.path().join("seal-cas-barrier");
    let sealer = {
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        let head = head.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            authenticated(&dir, &cap)
                .arg("seal")
                .arg(&head)
                .arg("--tag")
                .arg("race")
                .env("FORGEFS_TEST_SEAL_CAS_BARRIER", &barrier)
                .output()
                .expect("spawn racing forge seal")
        })
    };
    wait_for_parked_sealer(&barrier);

    // Move the head the parked sealer read. This is an ordinary authorised
    // checkin: nothing here is corrupt, contended or out of contract.
    write_and_checkin(d.path(), &root, &ns, "/b.txt", "second");
    let moved = ref_oid(d.path(), &root, &head).expect("head still exists");
    assert_ne!(
        observed, moved,
        "the fixture did not actually move the head; the window was never raced"
    );

    // Release the sealer: it now publishes (or refuses) with a stale read.
    std::fs::write(barrier.join("harness"), b"released").expect("release the seal barrier");
    let sealed = sealer.join().expect("join seal launcher");

    if sealed.status.success() {
        let named = sealed_commit(d.path(), &root, "race", "heads/probe-sealed");
        panic!(
            "seal published tags/race with exit 0 after {head} moved {observed} -> {moved}: the \
             tag names commit {named}, which the ref no longer holds (names the pre-race commit: \
             {}). A seal must CAS the ref it names (I5) and refuse a moved head, exit 4.",
            named == observed
        );
    }

    assert_eq!(
        sealed.status.code(),
        Some(4),
        "a head that moved under a seal is a stale observation (CLI_ABI.md exit 4); \
         stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    assert!(
        ref_oid(d.path(), &root, "tags/race").is_none(),
        "a refused seal must publish no tag"
    );
    assert_eq!(
        ref_oid(d.path(), &root, &head).as_deref(),
        Some(moved.as_str()),
        "a refused seal must not disturb the ref it read"
    );

    // The refusal is a clean one: the repository is intact and the caller can
    // simply seal what the ref holds now.
    run(authenticated(d.path(), &root).arg("fsck").arg("--full"));
    run(authenticated(d.path(), &root)
        .arg("seal")
        .arg(&head)
        .arg("--tag")
        .arg("race")
        .arg("--attest"));
    assert_eq!(
        sealed_commit(d.path(), &root, "race", "heads/probe-retry"),
        moved,
        "the retried seal must name exactly the commit the ref holds"
    );
}
