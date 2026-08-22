use forge_merge::three_way;
use forge_store::Store;
use forge_types::Error;
use tempfile::tempdir;

#[test]
fn blob_where_tree_expected_is_corruption_not_conflict() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let bad = store.put_blob_data(b"not a tree").unwrap();
    let good = store.empty_tree_id().unwrap();
    let err = three_way(&store, None, bad, good).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}
