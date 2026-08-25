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

/// I23: a collector must be able to make an object's absence observable in the
/// process that unlinked it.
///
/// The LRU caches assume immutability, which is true of an object's bytes and
/// false of its existence. Left alone they keep serving an object a sweep has
/// deleted, which hides exactly the bug a collector must not have: the
/// collecting process would pass its own reachability walk over bytes that are
/// no longer there.
#[test]
fn a_swept_object_is_gone_from_the_hot_cache_too() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let id = store.put_raw(b"bytes a sweep will reclaim").unwrap();

    // Warm the cache, then unlink the way a sweep does.
    assert_eq!(store.get_raw(id).unwrap(), b"bytes a sweep will reclaim");
    std::fs::remove_file(store.blobs.object_path(id)).unwrap();
    assert_eq!(
        store.get_raw(id).unwrap(),
        b"bytes a sweep will reclaim",
        "this is the hazard, not the bug: the hot path still serves an object that is gone"
    );

    store.forget_cached(id);
    assert!(
        store.get_raw(id).is_err(),
        "an unlinked object is still served from memory, so a sweep cannot make absence \
         observable without a cold reopen (I23)"
    );
}
