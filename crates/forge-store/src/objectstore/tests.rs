//! Both backends run the same contract, and `Store` runs end to end on the one
//! that is not the filesystem. That second run is the evidence that the seam is
//! a seam: if any local-filesystem assumption leaked out of `LocalBlobStore`,
//! this module would not compile.

use super::conformance::{assert_object_store_contract, ObjectStoreFixture};
use super::memory::MemoryObjectStore;
use super::{DurabilityClass, ObjectStore};
use crate::{LocalBlobStore, Meta, Store};
use forge_core::tree::{Tree, TreeStore};
use forge_types::ObjectId;
use parking_lot::Mutex;
use tempfile::TempDir;

#[derive(Default)]
struct LocalFixture {
    scratch: Mutex<Vec<TempDir>>,
}

impl ObjectStoreFixture for LocalFixture {
    type S = LocalBlobStore;

    fn name(&self) -> &'static str {
        "LocalBlobStore"
    }

    fn expected_class(&self) -> DurabilityClass {
        DurabilityClass::CrashDurable
    }

    fn writable(&self) -> LocalBlobStore {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path().to_path_buf()).unwrap();
        self.scratch.lock().push(dir);
        store
    }

    fn read_only(&self) -> LocalBlobStore {
        let dir = tempfile::tempdir().unwrap();
        drop(LocalBlobStore::new(dir.path().to_path_buf()).unwrap());
        let store = LocalBlobStore::open_read_only(dir.path().to_path_buf()).unwrap();
        self.scratch.lock().push(dir);
        store
    }

    fn corrupt(&self, store: &LocalBlobStore, id: ObjectId, bytes: &[u8]) -> bool {
        std::fs::write(store.object_path(id), bytes).unwrap();
        true
    }
}

#[derive(Default)]
struct MemoryFixture;

impl ObjectStoreFixture for MemoryFixture {
    type S = MemoryObjectStore;

    fn name(&self) -> &'static str {
        "MemoryObjectStore"
    }

    fn expected_class(&self) -> DurabilityClass {
        DurabilityClass::ProcessLifetime
    }

    fn writable(&self) -> MemoryObjectStore {
        MemoryObjectStore::new()
    }

    fn read_only(&self) -> MemoryObjectStore {
        MemoryObjectStore::new_read_only()
    }

    fn corrupt(&self, store: &MemoryObjectStore, id: ObjectId, bytes: &[u8]) -> bool {
        store.corrupt(id, bytes);
        true
    }
}

#[test]
fn local_blob_store_satisfies_the_object_store_contract() {
    assert_object_store_contract(&LocalFixture::default());
}

#[test]
fn memory_object_store_satisfies_the_object_store_contract() {
    assert_object_store_contract(&MemoryFixture);
}

/// The production object plane must never quietly drop to a weaker claim.
#[test]
fn the_production_object_plane_declares_crash_durability() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::new(dir.path().to_path_buf()).unwrap();
    assert_eq!(store.durability_class(), DurabilityClass::CrashDurable);
}

/// `Store` is written against the trait, not against files. Swapping the object
/// plane for one with no filesystem at all must leave the catalog-side
/// repository surface working.
#[test]
fn store_runs_end_to_end_on_a_non_filesystem_object_plane() {
    let dir = tempfile::tempdir().unwrap();
    let meta = Meta::open(&dir.path().join("meta.sqlite")).unwrap();
    let store = Store::with_object_store(dir.path().to_path_buf(), MemoryObjectStore::new(), meta);

    let payload = b"hello from a second backend";
    let blob = store.put_blob_data(payload).unwrap();
    assert_eq!(store.get_blob_data(blob).unwrap(), payload);
    // `put_blob_data` publishes `[frame_prefix, payload]` through the gather
    // primitive rather than building `Blob::encode()` (#320). Identity must not
    // depend on that: over a backend with no filesystem at all, the address is
    // still the one the buffered encoding would have produced (I2).
    assert_eq!(
        blob,
        forge_core::hash_bytes(
            &forge_core::object::Blob {
                data: payload.to_vec()
            }
            .encode()
        ),
        "a gathered publication must address the canonical buffered encoding"
    );

    let empty = store.empty_tree_id().unwrap();
    assert_eq!(store.reachable_oids(empty).unwrap(), vec![empty]);
    assert_eq!(store.root(), dir.path());

    // The batched publication path is the one checkin uses.
    let batch = store.begin_publish_batch();
    let batched = batch.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    batch.finish().unwrap();
    assert_eq!(batched, empty);

    assert!(store.blobs.has(empty));
    assert_eq!(
        store.blobs.durability_class(),
        DurabilityClass::ProcessLifetime,
        "a test backend must not masquerade as crash-durable"
    );
    assert!(store.stats().puts >= 2);
}
