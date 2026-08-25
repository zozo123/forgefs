use forge_store::Meta;
use forge_types::{CasResult, Error, ObjectId};
use tempfile::tempdir;

#[test]
fn cas_stats_classify_committed_outcomes() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("meta.sqlite")).unwrap();
    let a = ObjectId([1; 32]);
    let b = ObjectId([2; 32]);
    let c = ObjectId([3; 32]);
    meta.insert_ref("heads/test", a, "commit", false, false, "test", "init")
        .unwrap();

    let updated = meta
        .cas_ref("heads/test", a, b, "commit", "test", "test", false)
        .unwrap();
    assert!(matches!(updated, CasResult::Updated { .. }));
    let forked = meta
        .cas_ref("heads/test", a, c, "commit", "test", "test", false)
        .unwrap();
    assert!(matches!(forked, CasResult::Forked { .. }));

    meta.insert_ref("heads/protected", a, "commit", true, false, "test", "init")
        .unwrap();
    let denied = meta
        .cas_ref("heads/protected", a, b, "commit", "test", "test", false)
        .unwrap_err();
    assert!(matches!(denied, Error::Denied(_)));

    // Only committed CAS outcomes are counted; failed policy checks are separate.
    let stats = meta.stats();
    assert_eq!(stats.cas_updated, 1);
    assert_eq!(stats.cas_forked, 1);
    assert_eq!(stats.cas_denied, 1);
    // Five explicit `BEGIN IMMEDIATE` attempts were made and timed; the
    // protected-ref CAS was refused and rolled back, so only four of them
    // committed a write transaction. The counters answer different questions
    // and are not interchangeable -- see txn_count_autocommit.rs, where
    // autocommit writes pull them apart in the other direction.
    assert_eq!(stats.explicit_txn_count, 5);
    assert_eq!(stats.txn_count, 4);
    assert_eq!(stats.lock_acquires, 5);
    assert_eq!(
        stats.write_lock_acquires + stats.read_lock_acquires,
        stats.lock_acquires,
        "the summed lock pair must be exactly its two halves"
    );
    assert_eq!(
        stats.write_lock_wait_us + stats.read_lock_wait_us,
        stats.lock_wait_us
    );
    assert_eq!(
        stats.sqlite_accounted_us(),
        stats.lock_wait_us.saturating_add(stats.txn_us)
    );
}
