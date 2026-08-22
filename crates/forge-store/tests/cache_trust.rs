use forge_store::Store;
use forge_types::Error;
use tempfile::tempdir;

#[test]
fn verified_read_bypasses_warm_cache_after_disk_corruption() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let id = store.put_raw(b"immutable bytes").unwrap();

    assert_eq!(store.get_raw(id).unwrap(), b"immutable bytes");
    std::fs::write(store.blobs.object_path(id), b"corrupt backing bytes").unwrap();

    // The normal hot path may still serve immutable bytes verified earlier.
    assert_eq!(store.get_raw(id).unwrap(), b"immutable bytes");
    // Integrity boundaries must always reread and rehash durable storage.
    assert!(matches!(store.get_raw_verified(id), Err(Error::Corrupt(_))));
}
