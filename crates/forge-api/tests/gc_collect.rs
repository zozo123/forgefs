//! I19: collection unlinks only bytes nothing can reach.
//!
//! Issue #12/#309. `gc` used to plan and stop, so a repository grew without
//! bound. The hard part of collecting is not finding garbage, it is that
//! garbage stops being garbage while you look at it, and ForgeFS makes that
//! *more* likely than a mutable-name store does: objects are content
//! addressed, so a writer that reproduces bytes an object already holds does
//! not rewrite them (I3) and can start naming an object that has looked like
//! cold garbage for a month.
//!
//! These are the deterministic proofs of the three mechanisms that close it.
//! The proof that they compose under real concurrency is
//! `gc_collect_concurrent.rs`; a single-threaded "it deleted the right
//! objects" test cannot see the race at all.

use forge_api::{Forge, GC_COLLECT_MIN_AGE_FLOOR};
use forge_store::Store;
use forge_types::{Error, ObjectId};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    forge: Forge,
}

fn seeded() -> (Fixture, forge_cap::Cap) {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "shared").unwrap();
    let seed = forge.session_open(&root, "shared").unwrap();
    forge.mount(&root, &seed, "/", "ref:shared", true).unwrap();
    forge
        .write(&root, &seed, "/seed.txt", b"v0", false)
        .unwrap();
    forge.checkin(&root, &seed, "/", "seed").unwrap();
    (Fixture { _dir: dir, forge }, root)
}

fn object_path(root: &Path, id: ObjectId) -> PathBuf {
    let hex = id.hex();
    root.join("objects")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(&hex)
}

/// Make an object look as old as the age floor demands.
///
/// Sleeping through the floor would make every test here a multi-second test
/// for no extra proof: what is under test is which *age* the sweep reads and
/// when, never the wall clock itself. The concurrent soak uses real time.
fn backdate(path: &Path, by: Duration) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    let when = SystemTime::now() - by;
    file.set_modified(when).unwrap();
}

const OLD: Duration = Duration::from_secs(3_600);
const FLOOR: u64 = GC_COLLECT_MIN_AGE_FLOOR;

/// Mechanism 2, and the reason this task is hard. A writer that reproduces an
/// object's bytes legitimately does not rewrite them, so "written long ago"
/// says nothing about whether anyone is relying on the object right now. If a
/// deduplicating put does not refresh the object's age, the floor bounds
/// nothing for exactly the objects most at risk, and the sweep is entitled to
/// delete bytes a writer joined a millisecond ago.
#[test]
fn a_deduplicating_put_makes_an_object_young_again() {
    let (fx, root) = seeded();
    let f = &fx.forge;

    // An object no root names, aged past the floor. A second handle on the
    // same repository is the honest shape of the hazard: a writer that has put
    // bytes and has not yet published anything naming them.
    let writer = Store::open(f.root()).unwrap();
    let orphan = writer
        .put_raw(b"bytes a future writer will reproduce")
        .unwrap();
    let path = object_path(f.root(), orphan);
    backdate(&path, OLD);

    let planned = f.gc(&root, true, FLOOR).unwrap();
    assert!(
        planned.collectable_sample.contains(&orphan.hex()),
        "an unreachable object older than the floor is the whole premise: {planned:?}"
    );

    // A writer reproduces exactly those bytes. I3 means nothing is rewritten.
    let again = writer
        .put_raw(b"bytes a future writer will reproduce")
        .unwrap();
    assert_eq!(
        again, orphan,
        "content addressing: same bytes, same id (I2)"
    );

    let after = f.gc(&root, true, FLOOR).unwrap();
    assert!(
        !after.collectable_sample.contains(&orphan.hex()),
        "a deduplicating put did not protect {orphan}: gc still offers to collect bytes a \
         writer joined moments ago, which is the content-addressed sweep race (I19). \
         collectable={:?}",
        after.collectable_sample
    );
    assert!(
        after.withheld_young_objects >= 1,
        "the deduplicated object must be withheld as young, not silently dropped: {after:?}"
    );
}

/// Mechanism 3. The scan that shortlists candidates runs unlocked and can be
/// arbitrarily stale, so the age that decides an unlink must be re-read inside
/// the sweep transaction. Here the dedup lands after the object was already
/// old enough to be shortlisted.
#[test]
fn collection_rereads_the_age_it_decides_on() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let writer = Store::open(f.root()).unwrap();
    let orphan = writer
        .put_raw(b"joined between the scan and the unlink")
        .unwrap();
    let path = object_path(f.root(), orphan);
    backdate(&path, OLD);

    // Reproducing the bytes refreshes the age; a sweep that trusted a stale
    // stat would unlink it anyway.
    writer
        .put_raw(b"joined between the scan and the unlink")
        .unwrap();

    let swept = f.gc_collect(&root, FLOOR).unwrap();
    assert_eq!(
        swept.deleted_objects, 0,
        "collection unlinked an object whose age it had not re-read: {swept:?}"
    );
    assert!(
        path.exists(),
        "object {orphan} was unlinked despite a fresh dedup"
    );
}

/// Deleting the bytes is not the whole deletion (docs/GC.md, gap 3).
/// `object_intro` is an fsck root in both of its columns, so a collector that
/// unlinks bytes and leaves the provenance rows behind converts reclaimed
/// space into reported corruption. (The other half of that gap, the hot LRU
/// caches that would keep serving a deleted object inside the collecting
/// process, is pinned in `forge-store/tests/cache_trust.rs`.)
#[test]
fn collection_leaves_no_catalog_row_naming_swept_bytes() {
    let (fx, root) = seeded();
    let f = &fx.forge;

    // Fork a contribution, then retire it. Its closure -- commit, tree,
    // contribution and the blob only it introduced, all carrying object_intro
    // rows -- becomes unreachable.
    let winner = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &winner, "/", "ref:shared", true).unwrap();
    let loser = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &loser, "/", "ref:shared", true).unwrap();
    f.write(&root, &winner, "/w.txt", b"w", false).unwrap();
    let doomed = f
        .write(
            &root,
            &loser,
            "/l.txt",
            b"work that will be abandoned",
            false,
        )
        .unwrap();
    f.checkin(&root, &winner, "/", "winner").unwrap();
    let fork = match f.checkin(&root, &loser, "/", "loser").unwrap() {
        forge_types::CasResult::Forked { fork, .. } => fork,
        other => panic!("expected a fork (I18), got {other:?}"),
    };
    f.abandon_session(&root, &loser, true).unwrap();
    f.abandon_fork(&root, &fork).unwrap();

    let doomed_path = object_path(f.root(), doomed);
    assert!(doomed_path.exists());
    for entry in walk_objects(f.root()) {
        backdate(&entry, OLD);
    }

    let swept = f.gc_collect(&root, FLOOR).unwrap();
    assert!(
        swept.deleted_objects > 0,
        "an abandoned fork's closure is garbage and nothing else roots it: {swept:?}"
    );
    assert!(
        swept.collectable_sample.contains(&doomed.hex()),
        "the blob only the retired fork introduced must be among the swept: {swept:?}"
    );
    assert!(
        !doomed_path.exists(),
        "the blob only the retired fork introduced is still on disk: {doomed}"
    );

    let report = f.fsck(&root, true).unwrap();
    assert!(
        report.findings.is_empty(),
        "collection must not turn reclaimed space into reported corruption; \
         object_intro is an fsck root in both columns: {:?}",
        report.findings
    );
}

/// A sealed release's closure is the worst possible thing to sweep: it breaks
/// `verify` (I15) and there is no way back.
#[test]
fn a_sealed_release_survives_collection() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    f.seal(&root, "shared", "v1").unwrap();

    // Now move the ref on, so nothing but the seal roots the sealed closure.
    let s = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &s, "/", "ref:shared", true).unwrap();
    f.write(&root, &s, "/after.txt", b"later", false).unwrap();
    f.checkin(&root, &s, "/", "after the seal").unwrap();
    f.abandon_session(&root, &s, true).unwrap();
    for entry in walk_objects(f.root()) {
        backdate(&entry, OLD);
    }

    f.gc_collect(&root, FLOOR).unwrap();
    f.verify_tag(&root, "v1")
        .expect("collection swept part of a sealed release's closure (I15)");
    let report = f.fsck(&root, true).unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// A pinned base is reachable by definition (I8) even though no ref names it.
#[test]
fn a_live_sessions_pin_and_overlay_are_never_collected() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let ns = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &ns, "/", "ref:shared", true).unwrap();
    let staged = f
        .write(&root, &ns, "/staged.txt", b"not checked in", false)
        .unwrap();

    // Move the ref past the pin, so only the pin roots the pinned commit.
    let other = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &other, "/", "ref:shared", true).unwrap();
    f.write(&root, &other, "/other.txt", b"o", false).unwrap();
    f.checkin(&root, &other, "/", "move the ref").unwrap();
    f.abandon_session(&root, &other, true).unwrap();
    for entry in walk_objects(f.root()) {
        backdate(&entry, OLD);
    }

    let swept = f.gc_collect(&root, FLOOR).unwrap();
    assert!(
        !swept.collectable_sample.contains(&staged.hex()),
        "a staged overlay blob is a root: staged work is never garbage (I18): {swept:?}"
    );
    assert!(
        object_path(f.root(), staged).exists(),
        "staged blob {staged} was unlinked out from under a live session"
    );
    let report = f.fsck(&root, true).unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    // The pin still resolves, which is what I8 promises.
    f.read(&root, &ns, "/seed.txt").unwrap();
}

/// The floor is the only bound ForgeFS has on the window between a writer's
/// put and the transaction that names it, so it is refused below its minimum
/// rather than quietly honoured.
#[test]
fn collect_refuses_a_floor_below_the_minimum() {
    let (fx, root) = seeded();
    match fx.forge.gc_collect(&root, GC_COLLECT_MIN_AGE_FLOOR - 1) {
        Err(Error::Invalid(message)) => {
            assert!(
                message.contains("min-age-secs"),
                "the refusal must name the flag: {message}"
            );
        }
        other => panic!("a floor below the minimum must be refused, got {other:?}"),
    }
}

/// A filtered ref view is a filtered root set, and a filtered root set is
/// exactly how a collector deletes live objects (I13, I14).
#[test]
fn collect_denies_a_ref_scoped_capability() {
    let (fx, root) = seeded();
    let scoped = fx
        .forge
        .grant(&root, vec!["ops=read,write".into(), "ref=shared".into()])
        .unwrap();
    match fx.forge.gc_collect(&scoped, FLOOR) {
        Err(Error::Denied(_)) => {}
        other => panic!("a ref-scoped capability must not collect, got {other:?}"),
    }
}

fn walk_objects(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let objects = root.join("objects");
    let Ok(a_entries) = fs::read_dir(&objects) else {
        return out;
    };
    for a in a_entries.flatten() {
        if !a.path().is_dir() {
            continue;
        }
        for b in fs::read_dir(a.path()).unwrap().flatten() {
            if !b.path().is_dir() {
                continue;
            }
            for file in fs::read_dir(b.path()).unwrap().flatten() {
                if file.path().is_file() {
                    out.push(file.path());
                }
            }
        }
    }
    out
}
