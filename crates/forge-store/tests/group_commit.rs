//! Group commit for the metadata catalog (issue #49).
//!
//! I4/I6: a catalog write that returns success must already be durable, and a
//! ref plus its reflog must commit together. Group commit changes *how many*
//! `BEGIN IMMEDIATE ... COMMIT` transactions independent writers pay for and
//! nothing about when they are told they succeeded, so both properties below
//! are load-bearing:
//!
//! 1. concurrent writers really do share commits, otherwise the mutex still
//!    serialises one `synchronous=FULL` WAL fsync per write; and
//! 2. sharing a commit never lets one member's outcome contaminate another's,
//!    and never reports a success that a cold reopen cannot find.

use forge_store::{Meta, Observed};
use forge_types::{Error, ObjectId};
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::tempdir;

/// Concurrent independent writers must amortise the durable commit.
///
/// `txn_count` comes from SQLite's own commit hook, so it counts exactly the
/// write transactions SQLite committed -- one WAL fsync each under
/// `synchronous=FULL`. Without group commit every `overlay_upsert` opens its
/// own transaction and the count equals the number of writes exactly; the
/// assertion below is then an equality failing a strict `<`, which is why it
/// discriminates rather than merely passing more often.
#[test]
fn concurrent_writers_share_durable_commits() {
    let dir = tempdir().unwrap();
    let meta = Arc::new(Meta::open(&dir.path().join("meta.sqlite")).unwrap());
    let before = meta.stats().txn_count;

    let writers = 8usize;
    let each = 128usize;
    let gate = Arc::new(Barrier::new(writers));
    let mut handles = Vec::with_capacity(writers);
    for w in 0..writers {
        let meta = Arc::clone(&meta);
        let gate = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate.wait();
            for i in 0..each {
                meta.overlay_upsert(
                    "ns",
                    "/",
                    &format!("w{w}/f{i}"),
                    Some(ObjectId([7; 32])),
                    false,
                )
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let writes = (writers * each) as u64;
    let commits = meta.stats().txn_count - before;
    assert_eq!(
        meta.overlay_list("ns", "/").unwrap().len(),
        writes as usize,
        "every write must be present regardless of how they were batched"
    );
    assert!(
        commits < writes,
        "{writes} concurrent catalog writes cost {commits} durable SQLite commits: \
         independent writers are not sharing a commit, so each still pays its own \
         synchronous=FULL WAL fsync inside the write mutex (issue #49)"
    );
}

/// A rejected batch member must not take its neighbours down with it, and a
/// member told it succeeded must be findable after a cold reopen.
///
/// The reopen is the point. `Meta` answers reads from a live connection, so
/// asserting durability against the same handle that performed the write
/// proves nothing about what reached the WAL.
#[test]
fn a_rejected_batch_member_does_not_lose_its_neighbours() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let meta = Arc::new(Meta::open(&path).unwrap());

    // `a/b` makes the bare path `a` unrepresentable as a tree: any writer that
    // stages `a` must be rejected. It is staged first and alone so the
    // rejection below is guaranteed, not racy.
    meta.overlay_upsert("ns", "/", "a/b", Some(ObjectId([1; 32])), false)
        .unwrap();

    let writers = 8usize;
    let gate = Arc::new(Barrier::new(writers));
    let mut handles = Vec::with_capacity(writers);
    for w in 0..writers {
        let meta = Arc::clone(&meta);
        let gate = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate.wait();
            let mut accepted = Vec::new();
            for i in 0..64 {
                // Every writer interleaves work that must be refused with work
                // that must survive, so a doomed job is in flight while its
                // batch-mates are committing for the whole run.
                let refused = meta.overlay_upsert("ns", "/", "a", Some(ObjectId([2; 32])), false);
                assert!(
                    matches!(refused, Err(Error::Invalid(_))),
                    "staging `a` under `a/b` must be refused, got {refused:?}"
                );
                let path = format!("ok{w}/{i}");
                meta.overlay_upsert("ns", "/", &path, Some(ObjectId([3; 32])), false)
                    .unwrap();
                accepted.push(path);
            }
            accepted
        }));
    }
    let mut accepted: Vec<String> = Vec::new();
    for h in handles {
        accepted.extend(h.join().unwrap());
    }

    // Cold reopen: a fresh connection reading the durable file, not the caches
    // of the handle that acknowledged the writes.
    drop(meta);
    let reopened = Meta::open(&path).unwrap();
    let rows = reopened.overlay_list("ns", "/").unwrap();
    let found: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    for path in &accepted {
        assert!(
            found.contains(path.as_str()),
            "{path} was acknowledged as committed but is absent after a cold reopen (I4)"
        );
    }
    assert!(
        found.contains("a/b"),
        "the pre-existing entry must survive the rejected writers"
    );
    assert!(
        !found.contains("a"),
        "a refused write must leave nothing behind, even when its batch committed"
    );
    assert_eq!(
        found.len(),
        accepted.len() + 1,
        "the catalog must hold exactly the acknowledged writes plus `a/b`"
    );
}

/// I4/I9: independent first observations are hot catalog writes and must share
/// synchronous=FULL commits without losing a row or acknowledging volatile data.
#[test]
fn concurrent_observations_share_durable_commits_and_survive_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let meta = Arc::new(Meta::open(&path).unwrap());
    let before = meta.stats().txn_count;

    let writers = 8usize;
    let each = 128usize;
    let gate = Arc::new(Barrier::new(writers));
    let mut handles = Vec::with_capacity(writers);
    for w in 0..writers {
        let meta = Arc::clone(&meta);
        let gate = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate.wait();
            for i in 0..each {
                meta.observe(
                    "ns",
                    "/",
                    &format!("w{w}/f{i}"),
                    Observed::Blob(ObjectId([(w + 1) as u8; 32])),
                )
                .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let writes = (writers * each) as u64;
    let commits = meta.stats().txn_count - before;
    assert_eq!(
        meta.observations("ns").unwrap().len(),
        writes as usize,
        "every acknowledged observation must be present"
    );
    assert!(
        commits < writes,
        "{writes} concurrent observations cost {commits} durable SQLite commits: \
         observe() bypassed the group-commit lane"
    );

    drop(meta);
    let reopened = Meta::open(&path).unwrap();
    assert_eq!(
        reopened.observations("ns").unwrap().len(),
        writes as usize,
        "an acknowledged grouped observation disappeared after cold reopen (I4)"
    );
}
