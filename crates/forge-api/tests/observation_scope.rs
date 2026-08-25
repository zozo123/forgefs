//! I21/I9 (#328 follow-on): an authorised read may never make a session's own
//! work unpublishable.
//!
//! I19 gave every read-write mount its own pin so that a read and its later
//! validation consult the same tree. `check_observations` then had one rule
//! that did not follow the pin: it skipped an observation shadowed by the
//! session's own staged overlay ONLY when that observation belonged to the
//! mount currently being checked in.
//!
//! An observation records what a read SAW, and `read`/`ls` resolve through the
//! reading mount's overlay -- so a read of a path the session has staged
//! records the STAGED blob. Validating that against the mount's pinned tree,
//! which does not hold it, can never agree. Within one mount the skip hid this.
//! Across two writable mounts nothing did:
//!
//! ```text
//! mount /w1 rw ref:side
//! write /a.txt          # staged under `/`
//! read  /a.txt          # observation under `/` = the staged blob
//! write /w1/b.txt       # real work, under a different mount
//! checkin /w1           # StaleObservation at /:/a.txt, expected=<blob> found=absent
//! ```
//!
//! and no re-read could clear it, because re-reading records the staged blob
//! again. `abandon` refuses over the staged work, so `--discard-staged` was the
//! only exit -- the same wedge #328 had, reached without a protected ref, using
//! nothing but two writable mounts and a read-back of one's own write.
//!
//! The skip is now per OBSERVING mount, which is what I19's "resolved against
//! the mount's pin" meant all along.

use forge_api::Forge;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

/// I21. Reading back a path the session staged under ANOTHER mount must not
/// refuse the checkin of the mount that holds the real work.
#[test]
fn i21_reading_back_ones_own_write_does_not_wedge_another_mount() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "side").unwrap();

    let ns = f.session_open(&root, "main").unwrap();
    f.mount(&root, &ns, "/w1", "ref:side", true).unwrap();

    // Stage under `/`, then read it back through `/`: an authorised read,
    // through the session's own pinned read-write mount, of the session's own
    // staged bytes.
    f.write(&root, &ns, "/a.txt", b"mine", false).unwrap();
    assert_eq!(f.read(&root, &ns, "/a.txt").unwrap(), b"mine");
    // A directory listing is an observation too, and took the same path.
    f.ls(&root, &ns, "/").unwrap();

    // Independent work under the other writable mount.
    f.write(&root, &ns, "/w1/b.txt", b"other", false).unwrap();

    let published = f
        .checkin(&root, &ns, "/w1", "publish w1")
        .unwrap_or_else(|e| {
            panic!(
                "I21: an authorised read through the session's own read-write mount \
             made its work unpublishable: {e:?}.\n\
             The read recorded the overlay's blob under `/` and the checkin of \
             `/w1` compared it against `/`'s pinned tree, which does not hold \
             it. No re-read clears that -- re-reading records the same staged \
             blob -- and `abandon` refuses over staged work, so the only exit \
             destroys it."
            )
        });
    assert!(
        matches!(published, CasResult::Updated { .. }),
        "{published:?}"
    );

    // Both mounts still publish, and the session retires with no discard (I21).
    assert!(matches!(
        f.checkin(&root, &ns, "/", "publish root").unwrap(),
        CasResult::Updated { .. }
    ));
    f.abandon_session(&root, &ns, false)
        .expect("I21: a terminal state without discarding anything");
}

/// The same shape with a DELETE, which records `Absent` rather than a blob, so
/// the two directions of the comparison are both covered.
#[test]
fn i21_reading_back_ones_own_delete_does_not_wedge_another_mount() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "side").unwrap();

    // Give `/`'s own ref a file to delete.
    let seed = f.session_open(&root, "main").unwrap();
    f.write(&root, &seed, "/gone.txt", b"here", false).unwrap();
    let CasResult::Updated { name: base, .. } = f.checkin(&root, &seed, "/", "seed").unwrap()
    else {
        panic!("expected the session ref to advance")
    };
    f.abandon_session(&root, &seed, false).unwrap();

    let ns = f.session_open(&root, &base).unwrap();
    f.mount(&root, &ns, "/w1", "ref:side", true).unwrap();
    f.delete(&root, &ns, "/gone.txt").unwrap();
    // The read now sees the tombstone: Absent, against a base that HAS the file.
    assert!(matches!(
        f.read(&root, &ns, "/gone.txt").unwrap_err(),
        Error::NotFound(_)
    ));
    f.write(&root, &ns, "/w1/b.txt", b"other", false).unwrap();

    let published = f
        .checkin(&root, &ns, "/w1", "publish w1")
        .expect("I21: reading back one's own tombstone must not wedge another mount");
    assert!(
        matches!(published, CasResult::Updated { .. }),
        "{published:?}"
    );
    f.checkin(&root, &ns, "/", "publish root").unwrap();
    f.abandon_session(&root, &ns, false).unwrap();
}

/// The other direction, so the fix cannot have simply stopped checking: a
/// FOREIGN move -- a path this session did NOT stage -- must still be detected
/// (I9). This is what makes the skip above safe: it applies only to paths the
/// session's own overlay decides.
#[test]
fn i9_a_foreign_move_under_a_read_only_mount_is_still_stale() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();

    let a = f.session_open(&root, "main").unwrap();
    f.mount(&root, &a, "/ro", "ref:shared", false).unwrap();
    f.ls(&root, &a, "/ro").unwrap();
    f.write(&root, &a, "/mine.txt", b"work", false).unwrap();

    // B moves `shared` under A's read-only mount.
    let b = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &b, "/", "ref:shared", true).unwrap();
    f.write(&root, &b, "/new.txt", b"b", false).unwrap();
    f.checkin(&root, &b, "/", "advance").unwrap();

    let err = f
        .checkin(&root, &a, "/", "publish")
        .expect_err("I9: a foreign move under a live read-only mount is stale");
    let Error::StaleObservation { path, .. } = &err else {
        panic!("expected StaleObservation, got {err:?}");
    };
    assert_eq!(path, "/ro:/", "the refusal must name the mount that moved");

    // I21's other half: this refusal IS one a re-read clears.
    f.ls(&root, &a, "/ro").unwrap();
    assert!(
        f.checkin(&root, &a, "/", "publish after reread").is_ok(),
        "a stale refusal must be clearable by re-reading the path it names"
    );
}
