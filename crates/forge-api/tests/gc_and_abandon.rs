//! Issues #12 and #309: forks are GC roots until explicitly resolved, and
//! `abandon` is the verb that resolves one.
//!
//! I18 says a refused checkin never destroys staged work, and a losing CAS
//! forks the overlay onto `forks/<ref>/<agent>/<ulid>`. Those fork refs are
//! exactly the staged work I18 protects, so a reachability sweep must treat
//! them as roots -- which means nothing ever reclaims them and a contended
//! steady state grows without bound (one measured round: 1546 refs, 511 forks).
//!
//! These tests pin the resolution: a fork stays a root until `abandon` retires
//! it, retiring it is refused while any session can still resolve the name, the
//! retired name can never be resurrected, and `fsck --full` still passes over
//! the reflog tombstone that abandon leaves behind.

use forge_api::Forge;
use forge_types::{CasResult, Error, ObjectId};
use tempfile::{tempdir, TempDir};

const LOSER: &[u8] = b"work that lost the CAS and forked";

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

/// Drive one contended round and return (losing namespace, fork ref, the blob
/// only the losing contribution introduced).
fn forked_round(f: &Forge, cap: &forge_cap::Cap) -> (String, String, ObjectId) {
    let winner = f.session_open(cap, "shared").unwrap();
    f.mount(cap, &winner, "/", "ref:shared", true).unwrap();
    let loser = f.session_open(cap, "shared").unwrap();
    f.mount(cap, &loser, "/", "ref:shared", true).unwrap();

    f.write(cap, &winner, "/winner.txt", b"w", false).unwrap();
    let blob = f.write(cap, &loser, "/loser.txt", LOSER, false).unwrap();

    match f.checkin(cap, &winner, "/", "winner").unwrap() {
        CasResult::Updated { .. } => {}
        other => panic!("first checkin should win the CAS, got {other:?}"),
    }
    match f.checkin(cap, &loser, "/", "loser").unwrap() {
        CasResult::Forked { fork, .. } => (loser, fork, blob),
        other => panic!("second checkin should fork (I18), got {other:?}"),
    }
}

#[test]
fn an_unresolved_fork_is_a_gc_root_and_abandon_is_what_retires_it() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let (loser, fork, blob) = forked_round(f, &root);

    let before = f.gc(&root, true, 0).unwrap();
    assert_eq!(
        before.collectable_objects, 0,
        "an unresolved fork pins its whole closure, so nothing is collectable yet: {:?}",
        before.collectable_sample
    );
    assert_eq!(
        before.roots.unresolved_forks, 1,
        "the forked checkin must appear in the root set as an unresolved fork"
    );

    // The losing session still mounts the fork, so retiring it now would leave
    // a mount naming a ref that does not resolve -- the exact state fsck
    // reports as corruption (exit 2).
    match f.abandon_fork(&root, &fork) {
        Err(Error::Invalid(detail)) => assert!(
            detail.contains("mounted"),
            "expected a mounted-fork refusal, got {detail}"
        ),
        other => panic!("abandoning a mounted fork must be refused, got {other:?}"),
    }

    // Retire the session first (its pin also roots the fork commit), then the fork.
    f.abandon_session(&root, &loser, false).unwrap();
    let retired = f.abandon_fork(&root, &fork).unwrap();
    assert_eq!(retired.name, fork);

    let after = f.gc(&root, true, 0).unwrap();
    assert_eq!(
        after.roots.unresolved_forks, 0,
        "abandon must take the fork out of the root set"
    );
    assert!(
        after.collectable_sample.contains(&blob.hex()),
        "the abandoned fork's blob {} must become collectable; sample was {:?}",
        blob.hex(),
        after.collectable_sample
    );
    // blob + tree + contribution + commit at the very least.
    assert!(
        after.collectable_objects >= 4,
        "expected the whole abandoned closure to be collectable, got {}",
        after.collectable_objects
    );
    assert_eq!(after.deleted_objects, 0, "gc must never delete");

    // The tombstone abandon leaves behind is a reflog name with no ref row.
    // fsck --full reports that shape as REFLOG_ORPHAN unless it is taught that
    // an `abandon` chain is a deliberate retirement.
    let report = f.fsck(&root, true).unwrap();
    assert!(
        report.ok,
        "fsck must accept the abandon tombstone: {:?}",
        report.findings
    );
}

#[test]
fn a_retired_fork_name_can_never_be_recreated() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let (loser, fork, _) = forked_round(f, &root);
    f.abandon_session(&root, &loser, false).unwrap();
    f.abandon_fork(&root, &fork).unwrap();

    // Resurrecting the name would append an `old_oid IS NULL` reflog row on top
    // of a chain that already terminated, which audit_catalog reports as
    // REFLOG_CHAIN corruption on bytes that are entirely intact.
    match f.branch(&root, "main", &fork) {
        Err(Error::Invalid(detail)) => assert!(
            detail.contains("retired"),
            "expected a retired-name refusal, got {detail}"
        ),
        other => panic!("recreating a retired ref must be refused, got {other:?}"),
    }
    match f.abandon_fork(&root, &fork) {
        Err(Error::Invalid(detail)) => assert!(
            detail.contains("already abandoned"),
            "expected an already-abandoned refusal, got {detail}"
        ),
        other => panic!("double abandon must be refused, got {other:?}"),
    }
    assert!(f.fsck(&root, true).unwrap().ok);
}

#[test]
fn only_forks_may_be_abandoned() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    for name in ["main", "shared"] {
        match f.abandon_fork(&root, name) {
            Err(Error::Invalid(detail)) => assert!(
                detail.contains("forks/"),
                "expected a namespace refusal for {name}, got {detail}"
            ),
            other => panic!("abandoning {name} must be refused, got {other:?}"),
        }
    }
    match f.abandon_fork(&root, "forks/shared/anon/01ARZ3NDEKTSV4RRFFQ69G5FAV") {
        Err(Error::NotFound(_)) => {}
        other => panic!("abandoning a missing fork must be NotFound, got {other:?}"),
    }
}

#[test]
fn abandoning_a_session_never_discards_staged_work_by_accident() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let ns = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &ns, "/", "ref:shared", true).unwrap();
    let staged = f
        .write(&root, &ns, "/staged.txt", b"staged", false)
        .unwrap();

    match f.abandon_session(&root, &ns, false) {
        Err(Error::Invalid(detail)) => assert!(
            detail.contains("staged"),
            "expected a staged-work refusal, got {detail}"
        ),
        other => panic!("a session holding staged work must not be retired silently: {other:?}"),
    }
    assert_eq!(
        f.gc(&root, true, 0).unwrap().collectable_objects,
        0,
        "the refused abandon must leave the staged blob rooted by the overlay"
    );

    let retired = f.abandon_session(&root, &ns, true).unwrap();
    assert_eq!(retired.discarded_overlay, 1);
    let after = f.gc(&root, true, 0).unwrap();
    assert!(
        after.collectable_sample.contains(&staged.hex()),
        "the discarded overlay blob must become collectable, sample was {:?}",
        after.collectable_sample
    );
    assert!(f.fsck(&root, true).unwrap().ok);
}

#[test]
fn gc_reports_a_plan_and_refuses_to_collect() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    match f.gc(&root, false, 0) {
        Err(Error::Invalid(detail)) => assert!(
            detail.contains("dry-run"),
            "expected the collection-not-implemented refusal, got {detail}"
        ),
        other => panic!("gc must refuse to collect, got {other:?}"),
    }

    // The default floor withholds everything a fresh repository just wrote,
    // because an object is durable before the catalog row that roots it (I4).
    let (loser, fork, _) = forked_round(f, &root);
    f.abandon_session(&root, &loser, false).unwrap();
    f.abandon_fork(&root, &fork).unwrap();
    let guarded = f.gc(&root, true, forge_api::DEFAULT_MIN_AGE_SECS).unwrap();
    assert_eq!(
        guarded.collectable_objects, 0,
        "objects younger than the age floor must be withheld"
    );
    assert!(
        guarded.withheld_young_objects > 0,
        "the withheld set must account for them instead of dropping them"
    );
}

#[test]
fn gc_denies_a_ref_scoped_capability() {
    let (fx, root) = seeded();
    let f = &fx.forge;
    let scoped = f
        .grant(
            &root,
            vec!["ops=read,write".to_string(), "ref=shared".to_string()],
        )
        .unwrap();
    // A filtered ref list is a filtered root set, and a filtered root set is
    // how a collector deletes live objects.
    match f.gc(&scoped, true, 0) {
        Err(Error::Denied(_)) => {}
        other => panic!("gc must refuse a ref-scoped capability, got {other:?}"),
    }
}
