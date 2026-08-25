//! Shape audit: what I1-I18 do not say about a session holding more than one
//! read-write mount.
//!
//! I8 pins ONE base OID per session and says reads resolve "against that base
//! and its overlay". `Forge::session_mount_tree` implements that literally:
//! every read-write `ref:` mount is served from `namespaces.pinned_oid`,
//! whatever ref the mount actually names. Nothing in the set says a session may
//! hold only one writable base, so a second read-write `ref:` mount is accepted
//! and then read from the wrong tree entirely; nothing says checkin must
//! publish or refuse everything staged, so a session can be told `Noop` while
//! holding unpublished work on another mount; and nothing requires a session to
//! be able to make progress at all.
//!
//! The `#[test]`s here pin the behaviour that exists today so a change to it is
//! deliberate. The `#[ignore]`d companions state the proposed invariants
//! (I19/I20/I21 in the "Shape gaps" section of INVARIANTS.md) and fail today;
//! they are the regression tests a fix must flip on.

use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

/// Advance `r` by one checkin through a session of its own.
fn seed(f: &Forge, root: &Cap, r: &str, path: &str, data: &[u8]) {
    let ns = f.session_open(root, r).unwrap();
    f.mount(root, &ns, "/", &format!("ref:{r}"), true).unwrap();
    f.write(root, &ns, path, data, false).unwrap();
    let result = f.checkin(root, &ns, "/", "seed").unwrap();
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");
}

/// Two divergent refs plus a fresh session pinned to `base`.
fn diverged() -> (tempfile::TempDir, Forge, Cap) {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "base").unwrap();
    f.branch(&root, "main", "other").unwrap();
    seed(&f, &root, "base", "/a.txt", b"BASE");
    seed(&f, &root, "other", "/a.txt", b"OTHER");
    seed(&f, &root, "other", "/only-in-other.txt", b"x");
    (d, f, root)
}

/// Gap 1 (proposed I19). A read-write mount of a ref that is not the session's
/// own base is accepted and then answers from the session pin, so an authorised
/// read of `/other/a.txt` returns `ref:base`'s bytes and a file that exists only
/// in `ref:other` reads as absent. The mode alone decides which ref is read:
/// the identical mount taken read-only answers correctly.
#[test]
fn a_second_read_write_mount_serves_the_session_pin_not_the_ref_it_names() {
    let (_d, f, root) = diverged();

    let rw = f.session_open(&root, "base").unwrap();
    f.mount(&root, &rw, "/", "ref:base", true).unwrap();
    f.mount(&root, &rw, "/other", "ref:other", true).unwrap();

    assert_eq!(
        f.read(&root, &rw, "/other/a.txt").unwrap(),
        b"BASE",
        "a read-write mount of ref:other answered from the session pin"
    );
    assert!(
        matches!(
            f.read(&root, &rw, "/other/only-in-other.txt"),
            Err(Error::NotFound(_))
        ),
        "a file that exists in ref:other read as absent through a mount of ref:other"
    );

    let ro = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ro, "/other", "ref:other", false).unwrap();
    assert_eq!(f.read(&root, &ro, "/other/a.txt").unwrap(), b"OTHER");
}

/// Gap 2 (proposed I20). Checkin is scoped to one mount, so it can report
/// `Noop` -- "there was nothing to publish" -- while the session holds staged
/// work on another mount. I18 still holds: the work survives. But no ref names
/// it, `forge checkin` always passes `"/"`, and `abandon session` refuses with
/// advice ("check in first") that the CLI cannot carry out.
#[test]
fn checkin_reports_noop_while_another_mount_holds_unpublished_work() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(&root, &ns, "/other/staged.txt", b"precious", false)
        .unwrap();

    let result = f.checkin(&root, &ns, "/", "nothing here").unwrap();
    assert!(matches!(result, CasResult::Noop { .. }), "{result:?}");

    assert_eq!(
        f.read(&root, &ns, "/other/staged.txt").unwrap(),
        b"precious",
        "I18: the refused-to-publish overlay entry is still readable"
    );
    let error = f.abandon_session(&root, &ns, false).unwrap_err();
    assert!(
        matches!(error, Error::Invalid(ref m) if m.contains("staged overlay entries")),
        "{error:?}"
    );
}

/// Gap 2b (proposed I20), independent of how many writable refs a session may
/// hold. A read-write `oid:` mount accepts writes, but `checkin` of that mount
/// is refused unconditionally ("cannot checkin an oid mount"), so the entry is
/// staged into a mount no capability and no verb can ever publish -- while
/// `checkin` of `/` still reports `Noop`.
#[test]
fn a_read_write_oid_mount_accepts_writes_it_can_never_publish() {
    let (_d, f, root) = diverged();
    let (base_oid, _) = f.peel_commit("ref:base").unwrap();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    let spec = format!("oid:{}", base_oid.hex());
    f.mount(&root, &ns, "/snap", &spec, true).unwrap();
    f.write(&root, &ns, "/snap/notes.txt", b"work", false)
        .unwrap();

    let refused = f.checkin(&root, &ns, "/snap", "x").unwrap_err();
    assert!(
        matches!(refused, Error::Invalid(ref m) if m.contains("oid mount")),
        "{refused:?}"
    );
    let result = f.checkin(&root, &ns, "/", "x").unwrap();
    assert!(matches!(result, CasResult::Noop { .. }), "{result:?}");
    assert_eq!(f.read(&root, &ns, "/snap/notes.txt").unwrap(), b"work");
}

/// Gap 3 (proposed I21). One authorised read through the mount of gap 1 records
/// an observation against the pin, and `check_observations` validates that
/// mount against its live tree, so the two can never agree. Every checkin the
/// session attempts is refused with `StaleObservation`, re-reading does not
/// clear it, and the diagnostic names the observation rather than the mount
/// that read the wrong ref. Recovery exists but is not derivable from the
/// error: demote the mount to read-only and read again.
#[test]
fn one_read_through_that_mount_refuses_every_checkin_until_the_mount_is_demoted() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.read(&root, &ns, "/other/a.txt").unwrap();
    f.write(&root, &ns, "/w.txt", b"work", false).unwrap();

    for attempt in 0..3 {
        let error = f.checkin(&root, &ns, "/", "work").unwrap_err();
        assert!(
            matches!(error, Error::StaleObservation { .. }),
            "attempt {attempt}: {error:?}"
        );
        f.read(&root, &ns, "/other/a.txt").unwrap();
    }

    f.mount(&root, &ns, "/other", "ref:other", false).unwrap();
    f.read(&root, &ns, "/other/a.txt").unwrap();
    let result = f.checkin(&root, &ns, "/", "work").unwrap();
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");
}

/// Gap 4. `Meta::insert_mount` is `INSERT OR REPLACE` keyed on (ns, path), and
/// the overlay is keyed on (ns, mount path) with no record of the spec it was
/// staged against. Re-mounting a path therefore re-targets staged work at a
/// different ref without refusing, warning, or discarding it.
#[test]
fn remounting_a_path_silently_retargets_the_work_staged_against_the_old_spec() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(&root, &ns, "/other/s.txt", b"staged", false)
        .unwrap();
    f.mount(&root, &ns, "/other", "ref:base", true).unwrap();

    assert_eq!(
        f.read(&root, &ns, "/other/s.txt").unwrap(),
        b"staged",
        "the overlay staged against ref:other now hangs off a mount of ref:base"
    );
}

/// Gap 5. I9 says stale observations fail checkin, but not for how long an
/// observation constrains the session. `cas_ref_session` deletes observations
/// for the whole namespace while deleting overlay for the published mount only,
/// so a read through a foreign mount stops constraining the session at the
/// first checkin of any other mount.
#[test]
fn a_checkin_of_one_mount_forgets_every_other_mounts_observations() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/dep", "ref:other", false).unwrap();
    assert_eq!(f.read(&root, &ns, "/dep/a.txt").unwrap(), b"OTHER");

    f.write(&root, &ns, "/x.txt", b"one", false).unwrap();
    let first = f.checkin(&root, &ns, "/", "first").unwrap();
    assert!(matches!(first, CasResult::Updated { .. }), "{first:?}");

    seed(&f, &root, "other", "/a.txt", b"MOVED");

    f.write(&root, &ns, "/y.txt", b"two", false).unwrap();
    let second = f.checkin(&root, &ns, "/", "second").unwrap();
    assert!(
        matches!(second, CasResult::Updated { .. }),
        "the /dep read was forgotten by the first checkin: {second:?}"
    );
}

/// Proposed I19. A session has exactly one writable base, so a read-write mount
/// may name only the session's own live ref.
#[test]
#[ignore = "proposed I19: not true today; see the Shape gaps section of INVARIANTS.md"]
fn proposed_i19_a_read_write_mount_may_only_name_the_sessions_own_base() {
    let (_d, f, root) = diverged();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    let error = f
        .mount(&root, &ns, "/other", "ref:other", true)
        .expect_err("a second writable base must be refused, not silently mis-read");
    assert!(matches!(error, Error::Denied(_)), "{error:?}");
}

/// Proposed I20. Checkin publishes or explicitly refuses everything the session
/// staged; `Noop` means the session stages nothing anywhere.
#[test]
#[ignore = "proposed I20: not true today; see the Shape gaps section of INVARIANTS.md"]
fn proposed_i20_checkin_never_reports_noop_while_work_is_staged() {
    let (_d, f, root) = diverged();
    let (base_oid, _) = f.peel_commit("ref:base").unwrap();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    let spec = format!("oid:{}", base_oid.hex());
    f.mount(&root, &ns, "/snap", &spec, true).unwrap();
    f.write(&root, &ns, "/snap/notes.txt", b"work", false)
        .unwrap();
    assert!(
        !matches!(
            f.checkin(&root, &ns, "/", "nothing here"),
            Ok(CasResult::Noop { .. })
        ),
        "checkin reported Noop while /snap/notes.txt was staged in a mount no verb can publish"
    );
}

/// Proposed I21 (liveness). No sequence of authorised operations leaves a
/// session unable to reach a terminal state without discarding work.
#[test]
#[ignore = "proposed I21: not true today; see the Shape gaps section of INVARIANTS.md"]
fn proposed_i21_an_authorised_read_never_wedges_a_session() {
    let (_d, f, root) = diverged();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    if f.mount(&root, &ns, "/other", "ref:other", true).is_err() {
        // I19 refused the second writable base, so the wedging sequence is not
        // an authorised one and there is nothing left for I21 to rule out.
        return;
    }
    f.read(&root, &ns, "/other/a.txt").unwrap();
    f.write(&root, &ns, "/w.txt", b"work", false).unwrap();
    let result = f
        .checkin(&root, &ns, "/", "work")
        .expect("an authorised read must not make the session's own work unpublishable");
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");
}
