//! I19/I20/I21: what a session holding more than one read-write mount is
//! allowed to see, publish, and get stuck on.
//!
//! Before I19 a session had ONE pinned base and `Forge::session_mount_tree`
//! served EVERY read-write `ref:` mount from it, whatever ref the mount named.
//! That was right for the session's own ref -- the case #233 was fixed and
//! tested on -- and wrong for every other one: a read-write mount of
//! `ref:other` answered out of `ref:base`'s tree, a file present only in
//! `other` reported absent, the mount MODE silently decided which ref you read,
//! one authorised read through such a mount refused every later checkin
//! forever, and a checkin of it CASed `other` from a commit that was never in
//! `other`'s history. A read-write mount now records its OWN base, and reads,
//! observation checks and checkin for that mount all resolve against it.
//!
//! Every test here is a single-process, non-concurrent, fully authorised
//! command sequence. `e2e_concurrent.rs` and the concurrent proof in
//! `many_agent_soak.rs` cover contention.

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

fn read_ref(f: &Forge, root: &Cap, r: &str, path: &str) -> Vec<u8> {
    let ns = f.session_open(root, r).unwrap();
    f.mount(root, &ns, "/", &format!("ref:{r}"), false).unwrap();
    f.read(root, &ns, path).unwrap()
}

/// I19. A read-write mount answers out of the ref it names, and answers the
/// same as the identical mount taken read-only. This is the characterisation
/// that used to assert the opposite: the mode decided which ref you read, so an
/// agent computed on what it believed was another branch and then published.
#[test]
fn a_second_read_write_mount_serves_the_ref_it_names() {
    let (_d, f, root) = diverged();

    let rw = f.session_open(&root, "base").unwrap();
    f.mount(&root, &rw, "/", "ref:base", true).unwrap();
    f.mount(&root, &rw, "/other", "ref:other", true).unwrap();

    assert_eq!(
        f.read(&root, &rw, "/other/a.txt").unwrap(),
        b"OTHER",
        "a read-write mount of ref:other answered from the session pin"
    );
    assert_eq!(
        f.read(&root, &rw, "/other/only-in-other.txt").unwrap(),
        b"x",
        "a file that exists in ref:other read as absent through a mount of ref:other"
    );
    assert_eq!(f.read(&root, &rw, "/a.txt").unwrap(), b"BASE");

    // The mount mode is not allowed to change the answer.
    let ro = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ro, "/other", "ref:other", false).unwrap();
    assert_eq!(f.read(&root, &ro, "/other/a.txt").unwrap(), b"OTHER");
    assert_eq!(
        f.ls(&root, &rw, "/other").unwrap(),
        f.ls(&root, &ro, "/other").unwrap(),
        "the same mount listed different entries read-write than read-only"
    );
}

/// I19. Checkin of a mount CASes the ref THAT MOUNT names, from that mount's
/// own pin, and moves nothing else: not the other mount's ref, not the other
/// mount's base, not the session's own base.
#[test]
fn checkin_of_one_mount_publishes_to_its_own_ref_and_moves_no_other() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(
        &root,
        &ns,
        "/other/from-mount.txt",
        b"lands in other",
        false,
    )
    .unwrap();

    let published = f.checkin(&root, &ns, "/other", "publish other").unwrap();
    let CasResult::Updated { name, .. } = &published else {
        panic!("expected an update to ref:other, got {published:?}");
    };
    assert_eq!(name, "other");

    // The entry landed in `other`, and `other` still holds `other`'s content.
    assert_eq!(
        read_ref(&f, &root, "other", "/from-mount.txt"),
        b"lands in other"
    );
    assert_eq!(read_ref(&f, &root, "other", "/a.txt"), b"OTHER");
    // `base` was not touched, and did not acquire `other`'s entry.
    assert_eq!(read_ref(&f, &root, "base", "/a.txt"), b"BASE");
    let err = read_ref_err(&f, &root, "base", "/from-mount.txt");
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");

    // The session's own mount is unchanged, and still publishes to `base`.
    assert_eq!(f.read(&root, &ns, "/a.txt").unwrap(), b"BASE");
    f.write(&root, &ns, "/mine.txt", b"mine", false).unwrap();
    let own = f.checkin(&root, &ns, "/", "publish base").unwrap();
    let CasResult::Updated { name, .. } = &own else {
        panic!("expected an update to ref:base, got {own:?}");
    };
    assert_eq!(name, "base");
    assert_eq!(read_ref(&f, &root, "base", "/mine.txt"), b"mine");
    let err = read_ref_err(&f, &root, "other", "/mine.txt");
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

fn read_ref_err(f: &Forge, root: &Cap, r: &str, path: &str) -> Error {
    let ns = f.session_open(root, r).unwrap();
    f.mount(root, &ns, "/", &format!("ref:{r}"), false).unwrap();
    f.read(root, &ns, path).unwrap_err()
}

/// I5 is unchanged for a non-root mount. The mount's pin is the expected value,
/// so if the ref it names has moved the CAS loses and forks, and the fork
/// carries this mount's work (I18) instead of overwriting the winner.
#[test]
fn a_lost_race_on_a_non_root_mount_forks_and_keeps_the_work() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(&root, &ns, "/other/mine.txt", b"mine", false)
        .unwrap();

    // Someone else advances `other` after this mount pinned it.
    seed(&f, &root, "other", "/theirs.txt", b"theirs");

    let result = f.checkin(&root, &ns, "/other", "mine").unwrap();
    let CasResult::Forked {
        requested,
        fork,
        theirs,
        ..
    } = result
    else {
        panic!("a stale non-root mount must fork, not overwrite the winner");
    };
    assert_eq!(requested, "other");
    // #343: a session fork lands inside the losing agent's own scope, so the
    // capability that took the mount still covers the ref the mount is
    // retargeted at.
    assert_eq!(
        fork,
        "heads/agents/anon/forks/other/".to_string() + fork.rsplit('/').next().unwrap()
    );

    // The winner is intact and did NOT acquire this session's entry.
    assert_eq!(read_ref(&f, &root, "other", "/theirs.txt"), b"theirs");
    let err = read_ref_err(&f, &root, "other", "/mine.txt");
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    assert_ne!(
        f.peel_commit(&format!("ref:{fork}")).unwrap().0,
        theirs,
        "the fork must name this session's commit, not the winner's"
    );
    // I18: the work is durable under the fork, and the mount followed it.
    assert_eq!(f.read(&root, &ns, "/other/mine.txt").unwrap(), b"mine");
    assert_eq!(read_ref(&f, &root, &fork, "/mine.txt"), b"mine");
}

/// I22, composed with I19: checkin is scoped to ONE mount, so a checkin of `/`
/// has nothing of its own to publish while the session holds staged work on
/// `/other`. It used to answer `Noop` -- "there was nothing to publish" -- which
/// is the one sentence that may not be said over work that exists (#326). It now
/// refuses, exit 1, and names the mount. I19 is what makes that refusal
/// actionable rather than a wedge: `checkin /other` publishes the work to
/// `ref:other`, from `/other`'s own pin.
#[test]
fn checkin_refuses_a_noop_while_another_mount_holds_unpublished_work() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(&root, &ns, "/other/staged.txt", b"precious", false)
        .unwrap();

    let error = f
        .checkin(&root, &ns, "/", "nothing here")
        .expect_err("I22: a noop may not be reported over work the session holds");
    assert!(
        matches!(error, Error::Invalid(ref m) if m.contains("/other (1 staged entry)")),
        "the refusal must name the mount that holds the work: {error:?}"
    );

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

    // I20: the mount that accepted the write has a verb that publishes it.
    let published = f.checkin(&root, &ns, "/other", "publish it").unwrap();
    assert!(
        matches!(published, CasResult::Updated { .. }),
        "{published:?}"
    );
    assert_eq!(read_ref(&f, &root, "other", "/staged.txt"), b"precious");
}

/// I20. A read-write `oid:` mount used to be accepted and then to accept
/// writes, while `checkin` of it was refused unconditionally, so the entry was
/// staged where no capability and no verb could ever publish it and `abandon`
/// demanded a checkin the CLI could not perform. An immutable spec has no ref
/// to advance, so the mount itself is refused: no write path without a publish
/// path. `fsck` also reported such a row as MOUNT_RW_OID corruption, so this
/// closes an authorised way to manufacture a corruption finding on intact bytes.
#[test]
fn a_read_write_oid_mount_is_refused_at_mount_time() {
    let (_d, f, root) = diverged();
    let (base_oid, _) = f.peel_commit("ref:base").unwrap();
    let spec = format!("oid:{}", base_oid.hex());

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();

    let refused = f.mount(&root, &ns, "/snap", &spec, true).unwrap_err();
    assert!(matches!(refused, Error::Denied(_)), "{refused:?}");
    let text = refused.to_string();
    assert!(
        text.contains("read-write") && text.contains("published"),
        "the diagnostic must say why an oid mount cannot be writable: {text}"
    );

    // Read-only is the supported way to pin a frozen tree, and still works.
    f.mount(&root, &ns, "/snap", &spec, false).unwrap();
    assert_eq!(f.read(&root, &ns, "/snap/a.txt").unwrap(), b"BASE");
    let denied = f
        .write(&root, &ns, "/snap/notes.txt", b"work", false)
        .unwrap_err();
    assert!(matches!(denied, Error::Denied(_)), "{denied:?}");

    // A read-write mount of a ref that does not hold a commit is refused for
    // the same reason: checkin CASes a commit ref or nothing.
    f.seal(&root, "base", "frozen").unwrap();
    let refused = f
        .mount(&root, &ns, "/tag", "ref:tags/frozen", true)
        .unwrap_err();
    assert!(matches!(refused, Error::Denied(_)), "{refused:?}");
}

/// I21. One authorised read through a second read-write mount used to record an
/// observation against the session pin while `check_observations` validated
/// that mount against its LIVE ref, so the two could never agree and every
/// checkin the session attempted was refused with `StaleObservation` forever.
/// Re-reading did not clear it and the diagnostic named the observation rather
/// than the mount, so the escape was not derivable from the error. The read and
/// the check now consult the same tree.
#[test]
fn a_read_through_a_second_read_write_mount_does_not_wedge_the_session() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.read(&root, &ns, "/other/a.txt").unwrap();
    f.ls(&root, &ns, "/other").unwrap();
    f.read(&root, &ns, "/other/does-not-exist.txt").unwrap_err();
    f.write(&root, &ns, "/w.txt", b"work", false).unwrap();

    let result = f.checkin(&root, &ns, "/", "work").unwrap();
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");

    // And the pinned read-write mount does not go stale when its ref moves
    // under it, because that is what the pin is for (I8/I19).
    f.read(&root, &ns, "/other/a.txt").unwrap();
    f.write(&root, &ns, "/other/mine.txt", b"mine", false)
        .unwrap();
    seed(&f, &root, "other", "/theirs.txt", b"theirs");
    let result = f.checkin(&root, &ns, "/other", "mine").unwrap();
    assert!(
        matches!(result, CasResult::Forked { .. }),
        "a moved ref must fork on CAS, not refuse the read: {result:?}"
    );
}

/// I19. `insert_mount` is `INSERT OR REPLACE` on (ns, path) while the overlay
/// is keyed on (ns, mount path), so re-mounting a path used to re-aim
/// everything staged under it at another ref with no refusal, warning, or
/// discard. The mount row now records the spec the overlay was staged against,
/// which is what makes the collision detectable.
#[test]
fn remounting_a_path_over_staged_work_is_refused() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.write(&root, &ns, "/other/s.txt", b"staged", false)
        .unwrap();

    let refused = f.mount(&root, &ns, "/other", "ref:base", true).unwrap_err();
    assert!(matches!(refused, Error::Invalid(_)), "{refused:?}");
    let text = refused.to_string();
    assert!(
        text.contains("/other") && text.contains("ref:other") && text.contains("1 staged entry"),
        "the diagnostic must name the mount, the spec it was staged against, and how much: {text}"
    );

    // Demoting a mount that holds staged work is refused for the same reason:
    // checkin refuses a read-only mount, so the work would be unpublishable.
    let refused = f
        .mount(&root, &ns, "/other", "ref:other", false)
        .unwrap_err();
    assert!(matches!(refused, Error::Invalid(_)), "{refused:?}");

    // I18: nothing was discarded, and the supported exit still works.
    assert_eq!(f.read(&root, &ns, "/other/s.txt").unwrap(), b"staged");
    let published = f.checkin(&root, &ns, "/other", "publish").unwrap();
    assert!(
        matches!(published, CasResult::Updated { .. }),
        "{published:?}"
    );
    assert_eq!(read_ref(&f, &root, "other", "/s.txt"), b"staged");

    // With nothing staged the re-mount is allowed, and re-taking the same spec
    // stays idempotent.
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:base", true).unwrap();
    assert_eq!(f.read(&root, &ns, "/other/a.txt").unwrap(), b"BASE");
}

/// I9's epoch is the MOUNT that recorded the read, not the session (#329).
///
/// This is the test that used to assert the opposite. `cas_ref_session` cleared
/// `observations` for the whole namespace while clearing `overlay` for the
/// published mount alone, so a read through a foreign mount stopped
/// constraining the session at the first checkin of any OTHER mount -- and the
/// overlay that read justified outlived the observation that justified it.
///
/// Per-mount is the only epoch the rest of the system can carry. The
/// per-session alternative has to clear the whole namespace's OVERLAY with the
/// observations to be coherent, and that destroys another mount's staged work,
/// which I18 forbids outright. Under I19 every read-write mount already owns
/// its own pin, its own overlay and its own CAS, so the observation is the last
/// piece of session state that was not scoped the way everything around it is.
#[test]
fn i9_a_checkin_of_one_mount_keeps_every_other_mounts_observations() {
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
    let second = f.checkin(&root, &ns, "/", "second").unwrap_err();
    let Error::StaleObservation { path, .. } = &second else {
        panic!("the /dep read must still constrain the session: {second:?}");
    };
    assert_eq!(path, "/dep:/a.txt", "{second:?}");

    // I21: the refusal is one a re-read clears, and nothing was discarded.
    assert_eq!(f.read(&root, &ns, "/dep/a.txt").unwrap(), b"MOVED");
    let third = f.checkin(&root, &ns, "/", "third").unwrap();
    assert!(matches!(third, CasResult::Updated { .. }), "{third:?}");
    assert_eq!(read_ref(&f, &root, "base", "/y.txt"), b"two");
}

/// The other half of the same epoch (#329): a checkin DOES clear the
/// observations of the mount it publishes, in the same transaction that clears
/// that mount's overlay and re-pins it.
///
/// Without this half, "never forget an observation" would wedge the session
/// that read a path and then wrote it: publication makes the mount's own tree
/// disagree with the observation the overlay had been shadowing, and no re-read
/// could have prevented a refusal it did not cause (I21).
#[test]
fn i9_a_checkin_clears_the_observations_of_the_mount_it_publishes() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    assert_eq!(f.read(&root, &ns, "/a.txt").unwrap(), b"BASE");
    f.write(&root, &ns, "/a.txt", b"NEW", false).unwrap();
    let first = f.checkin(&root, &ns, "/", "first").unwrap();
    assert!(matches!(first, CasResult::Updated { .. }), "{first:?}");

    f.write(&root, &ns, "/b.txt", b"two", false).unwrap();
    let second = f.checkin(&root, &ns, "/", "second").unwrap();
    assert!(matches!(second, CasResult::Updated { .. }), "{second:?}");
    assert_eq!(read_ref(&f, &root, "base", "/a.txt"), b"NEW");
}

/// I9 still holds for a read-only mount: it resolves live on purpose, so a read
/// through one that the ref then moves under DOES refuse the next checkin. I19
/// must not have flattened that into "nothing ever goes stale".
#[test]
fn a_read_only_mount_still_goes_stale_when_its_ref_moves() {
    let (_d, f, root) = diverged();

    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/dep", "ref:other", false).unwrap();
    assert_eq!(f.read(&root, &ns, "/dep/a.txt").unwrap(), b"OTHER");
    f.write(&root, &ns, "/x.txt", b"one", false).unwrap();

    seed(&f, &root, "other", "/a.txt", b"MOVED");

    let error = f.checkin(&root, &ns, "/", "stale dep").unwrap_err();
    assert!(matches!(error, Error::StaleObservation { .. }), "{error:?}");
    // I18: refused, and the work survives; re-reading clears the observation.
    assert_eq!(f.read(&root, &ns, "/x.txt").unwrap(), b"one");
    assert_eq!(f.read(&root, &ns, "/dep/a.txt").unwrap(), b"MOVED");
    let result = f.checkin(&root, &ns, "/", "after re-read").unwrap();
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");
}

/// I19, stated whole: every read-write mount has its own base, reads resolve
/// against it, and checkin CASes the ref that mount names from that base. This
/// is the regression test the fix had to flip on; the proposal it replaces
/// ("a read-write mount may name only the session's own live ref") would have
/// refused the mount instead, which forbids the useful case rather than fixing
/// it and leaves an agent no way to write two branches from one session.
#[test]
fn i19_every_read_write_mount_resolves_against_its_own_base() {
    let (_d, f, root) = diverged();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();

    // Each mount reads its own ref, not the session's base.
    assert_eq!(f.read(&root, &ns, "/a.txt").unwrap(), b"BASE");
    assert_eq!(f.read(&root, &ns, "/other/a.txt").unwrap(), b"OTHER");

    // Each mount is pinned: a later move of either ref changes neither read.
    seed(&f, &root, "base", "/a.txt", b"BASE-MOVED");
    seed(&f, &root, "other", "/a.txt", b"OTHER-MOVED");
    assert_eq!(f.read(&root, &ns, "/a.txt").unwrap(), b"BASE");
    assert_eq!(f.read(&root, &ns, "/other/a.txt").unwrap(), b"OTHER");

    // And each publishes to its own ref -- here both lose the race, so both
    // fork, and neither ref ends up holding the other's content.
    f.write(&root, &ns, "/mine.txt", b"in base", false).unwrap();
    f.write(&root, &ns, "/other/mine.txt", b"in other", false)
        .unwrap();
    for (mount, expected) in [("/", "base"), ("/other", "other")] {
        let result = f.checkin(&root, &ns, mount, "mine").unwrap();
        let CasResult::Forked {
            requested, fork, ..
        } = result
        else {
            panic!("{mount}: expected a fork, got {result:?}");
        };
        assert_eq!(&requested, expected);
        assert!(
            fork.starts_with(&format!("heads/agents/anon/forks/{expected}/")),
            "#343: the fork must land in the losing agent's own scope: {fork}"
        );
    }
    assert_eq!(read_ref(&f, &root, "base", "/a.txt"), b"BASE-MOVED");
    assert_eq!(read_ref(&f, &root, "other", "/a.txt"), b"OTHER-MOVED");
}

/// I20. Every mount that accepts a write has a verb that can publish it, so
/// there is no way to stage work into a mount nothing can reach.
#[test]
fn i20_a_mount_that_accepts_a_write_can_always_publish_it() {
    let (_d, f, root) = diverged();
    let (base_oid, _) = f.peel_commit("ref:base").unwrap();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();

    // The shape that had no publish path is refused at mount time.
    let spec = format!("oid:{}", base_oid.hex());
    let refused = f.mount(&root, &ns, "/snap", &spec, true).unwrap_err();
    assert!(matches!(refused, Error::Denied(_)), "{spec}: {refused:?}");

    // Every read-write mount that IS accepted publishes what it accepted.
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();
    for (mount, path) in [("/", "/w.txt"), ("/other", "/other/w.txt")] {
        f.write(&root, &ns, path, b"work", false).unwrap();
        let result = f.checkin(&root, &ns, mount, "publish").unwrap();
        assert!(
            matches!(result, CasResult::Updated { .. }),
            "{mount}: {result:?}"
        );
    }
    // Nothing is left staged, so the session can also just be retired.
    f.abandon_session(&root, &ns, false)
        .expect("a session that published everything must be retirable");
}

/// I21 (liveness). No sequence of authorised reads leaves a session unable to
/// reach a terminal state: publish, fork, or abandon.
#[test]
fn i21_an_authorised_read_never_wedges_a_session() {
    let (_d, f, root) = diverged();
    let ns = f.session_open(&root, "base").unwrap();
    f.mount(&root, &ns, "/", "ref:base", true).unwrap();
    f.mount(&root, &ns, "/other", "ref:other", true).unwrap();

    // Read everything reachable through both writable mounts, in both shapes a
    // read comes in (blob, directory, absent), then do the session's own work.
    for path in [
        "/a.txt",
        "/other/a.txt",
        "/other/only-in-other.txt",
        "/other/absent.txt",
    ] {
        let _ = f.read(&root, &ns, path);
    }
    f.ls(&root, &ns, "/").unwrap();
    f.ls(&root, &ns, "/other").unwrap();
    f.write(&root, &ns, "/w.txt", b"work", false).unwrap();

    let result = f
        .checkin(&root, &ns, "/", "work")
        .expect("an authorised read must not make the session's own work unpublishable");
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");

    // Terminal state reached for the whole session: nothing staged anywhere,
    // so `abandon` retires it without discarding work.
    f.abandon_session(&root, &ns, false)
        .expect("a session with nothing staged must be retirable");
}
