//! I8: a session pinned to a base OID must read from that base, not from a
//! live ref another agent can move. I18: a refused checkin must leave staged
//! work readable, while a lost CAS must preserve it in a durable fork. The I8
//! regression was authored in #245; Forge::session_mount_tree owns that fix.
//!
use forge_api::Forge;
use forge_store::Store;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

#[test]
fn rw_mount_reads_pinned_base_then_lost_race_forks() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    f.branch(&root, "main", "shared").unwrap();

    // Seed shared with a stable value.
    let seed = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &seed, "/", "ref:shared", true).unwrap();
    f.write(&root, &seed, "/a.txt", b"v0", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &seed, "/", "seed").unwrap(),
        CasResult::Updated { .. }
    ));

    // A pins shared@v0.
    let a = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &a, "/", "ref:shared", true).unwrap();

    // B advances the mutable ref to v1 after A opened.
    let b = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &b, "/", "ref:shared", true).unwrap();
    f.write(&root, &b, "/a.txt", b"v1", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &b, "/", "advance").unwrap(),
        CasResult::Updated { .. }
    ));

    // I8: A's workspace reads must come from its pinned base, never the live ref.
    assert_eq!(f.read(&root, &a, "/a.txt").unwrap(), b"v0");

    // A's disjoint staged work is still valid. The shared-ref race is resolved by
    // the normal CAS loser path: preserve A's work under an explicit fork.
    f.write(&root, &a, "/b.txt", b"mine", false).unwrap();
    let result = f.checkin(&root, &a, "/", "mine").unwrap();
    assert!(matches!(result, CasResult::Forked { .. }), "{result:?}");

    // The session remains usable after the fork rather than becoming permanently
    // wedged by an observation that can never match its pinned base.
    assert_eq!(f.read(&root, &a, "/a.txt").unwrap(), b"v0");
    assert_eq!(f.read(&root, &a, "/b.txt").unwrap(), b"mine");
}

/// I18, on the one shape that can still produce a Denied checkin: a catalog
/// row written BEFORE I20 refused a read-write mount of a protected ref.
///
/// `Forge::mount` no longer creates this row (#328), and no verb can protect a
/// ref after a mount of it was taken -- `mount_protection.rs` pins that closure
/// property -- so the only way to hold one is to have opened the session on an
/// older build. The refusal deep in `cas_ref_session` is kept as the
/// fail-closed floor for exactly that case, and this test is what keeps the
/// floor honest: it builds the legacy row directly in the catalog, through a
/// cold reopen so nothing is served out of the live `Forge`'s caches.
///
/// This is also the half of #328 the mount-time refusal does NOT close, stated
/// rather than implied: such a session still has no exit but
/// `--discard-staged`. What I18 guarantees for it is only that the work stays
/// READABLE until someone chooses to destroy it.
#[test]
fn a_pre_i20_mount_of_a_protected_ref_still_fails_closed_without_losing_work() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "writable").unwrap();

    let session = f.session_open(&root, "writable").unwrap();
    f.mount(&root, &session, "/w", "ref:writable", true)
        .unwrap();
    f.write(&root, &session, "/w/kept.txt", b"still staged", false)
        .unwrap();
    let (main_oid, _) = f.peel_commit("main").unwrap();
    drop(f);

    // The row an older build would have written: read-write, pinned, naming a
    // protected ref. `Forge::mount` refuses to produce it now.
    {
        let store = Store::open(&d.path().join(".forge")).unwrap();
        store
            .meta
            .insert_mount(&session, "/w", "ref:main", "rw", Some(main_oid))
            .unwrap();
    }

    let f = Forge::open(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let error = f
        .checkin(&root, &session, "/w", "protected ref must refuse")
        .unwrap_err();
    assert!(
        matches!(error, Error::Denied(_)),
        "I5: a protected ref denies the session CAS, and that floor must stay \
         reachable for a row `mount` can no longer create: {error:?}"
    );
    assert_eq!(
        f.read(&root, &session, "/w/kept.txt").unwrap(),
        b"still staged",
        "I18: a refused checkin discarded the overlay"
    );
    // And this is the residual #328 does not close: the work survives, but the
    // only exit still destroys it.
    assert!(f.abandon_session(&root, &session, false).is_err());
    assert_eq!(
        f.abandon_session(&root, &session, true)
            .unwrap()
            .discarded_overlay,
        1
    );
}

#[test]
fn i18_stale_observation_refusal_keeps_staged_work() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    f.branch(&root, "main", "shared").unwrap();

    // Seed shared with a value another session can observe.
    let seed = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &seed, "/", "ref:shared", true).unwrap();
    f.write(&root, &seed, "/a.txt", b"v0", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &seed, "/", "seed").unwrap(),
        CasResult::Updated { .. }
    ));

    // The session under test observes shared through a live read-only mount and
    // stages unrelated work in its own read-write mount.
    let session = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &session, "/live", "ref:shared", false)
        .unwrap();
    assert_eq!(f.read(&root, &session, "/live/a.txt").unwrap(), b"v0");
    f.write(&root, &session, "/kept.txt", b"still staged", false)
        .unwrap();

    // Someone else moves shared, so the recorded observation can no longer hold.
    let other = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &other, "/", "ref:shared", true).unwrap();
    f.write(&root, &other, "/a.txt", b"v1", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &other, "/", "advance").unwrap(),
        CasResult::Updated { .. }
    ));

    let error = f
        .checkin(&root, &session, "/", "stale read must refuse")
        .unwrap_err();
    assert!(matches!(error, Error::StaleObservation { .. }), "{error:?}");

    // I18: this refusal publishes nothing, so it must leave the overlay intact.
    let kept = f
        .read(&root, &session, "/kept.txt")
        .expect("a checkin refused for a stale observation discarded the overlay");
    assert_eq!(kept, b"still staged");
}
