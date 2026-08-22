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
    assert!(stats.txn_us > 0);
}
