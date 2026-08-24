//! Local content-addressed objects + SQLite mutable surface.

pub mod blob;
mod graph;
pub mod meta;
mod metrics;

pub use blob::{LocalBlobStore, PublishBatch};
pub use graph::{
    decode_graph_object, DecodedGraphObject, GraphEdge, GraphExpectation, VerifiedGraphObject,
    MAX_GRAPH_OBJECTS,
};
pub use meta::{
    sanitize_agent, CatalogAudit, CatalogObjectExpectation, CheckpointResult, DurabilityPolicy,
    Meta, MetaStats, MountRow, NsRow, OverlayRow, CURRENT_SCHEMA_VERSION,
};

use forge_core::object::{decode_object_type, Blob, Commit, Conflict, Snapshot};
use forge_core::tree::{Tree, TreeStore};
use forge_core::Contribution;
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Stable names for durability transitions exercised by the debug-only fault
/// injector. The enum is present in release builds so the real barrier calls
/// stay identical, but injection state and branches are compiled out.
#[doc(hidden)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityBarrier {
    ObjectFile,
    ObjectFileAfter,
    ObjectExistingFile,
    ObjectLinkAfter,
    ObjectPathDirectory,
    ObjectPublicationDirectory,
    ObjectPublicationDirectoryAfter,
    ObjectTemporaryDirectory,
    InitFile,
    InitKeyDirectory,
    InitCleanupDirectory,
    InitStagingDirectory,
    InitPublicationDirectory,
    InitParentDirectory,
    OpenPublicationDirectory,
    MetadataRefCommitAfter,
    MetadataCheckpointBefore,
    MetadataCheckpointAfter,
    OtherFile,
    OtherDirectory,
}

/// Deterministic, current-thread barrier failures for debug/test builds.
///
/// This is deliberately an explicitly armed in-process seam: it reads no
/// environment variables, has no process-exit path, and is absent from release
/// builds. Keeping plans thread-local also prevents concurrently executing
/// tests from perturbing one another.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod barrier_fault {
    use super::DurabilityBarrier;
    use std::cell::Cell;
    use std::marker::PhantomData;
    use std::rc::Rc;

    #[derive(Clone, Copy, Debug)]
    struct Plan {
        point: DurabilityBarrier,
        occurrence: usize,
        matches: usize,
        fired: bool,
    }

    thread_local! {
        static ACTIVE: Cell<Option<Plan>> = const { Cell::new(None) };
    }

    /// An RAII fault scope. Dropping it removes the current thread's plan even
    /// when the operation under test panics.
    pub struct Guard {
        // A plan belongs to the thread that armed it. Make moving its guard to
        // another thread a compile-time error.
        _thread_local: PhantomData<Rc<()>>,
    }

    impl Guard {
        pub fn fired(&self) -> bool {
            ACTIVE.with(|active| active.get().is_some_and(|plan| plan.fired))
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(None));
        }
    }

    /// Fail the selected one-based occurrence of `point` on this thread.
    pub fn fail_at(point: DurabilityBarrier, occurrence: usize) -> Guard {
        assert!(occurrence > 0, "barrier occurrence is one-based");
        ACTIVE.with(|active| {
            assert!(
                active.get().is_none(),
                "a barrier fault plan is already armed"
            );
            active.set(Some(Plan {
                point,
                occurrence,
                matches: 0,
                fired: false,
            }));
        });
        Guard {
            _thread_local: PhantomData,
        }
    }

    pub(super) fn hit(point: DurabilityBarrier) -> bool {
        ACTIVE.with(|active| {
            let Some(mut plan) = active.get() else {
                return false;
            };
            if point != plan.point || plan.fired {
                return false;
            }
            plan.matches += 1;
            let should_fail = plan.matches == plan.occurrence;
            if should_fail {
                plan.fired = true;
            }
            active.set(Some(plan));
            should_fail
        })
    }
}

#[inline]
fn inject_barrier_failure(point: DurabilityBarrier) -> Result<()> {
    #[cfg(debug_assertions)]
    if barrier_fault::hit(point) {
        return Err(Error::Io(format!(
            "injected durability barrier failure at {point:?}"
        )));
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
    Ok(())
}

/// Complete the strongest durability barrier supported by the platform.
/// macOS `fsync(2)` does not flush device caches, so match SQLite's
/// `fullfsync=ON` policy with `F_FULLFSYNC` for immutable objects and bootstrap
/// files too. Failure is fatal: a weaker object plane could violate I4 after a
/// power loss even while the SQLite ref survives.
pub fn durable_sync_file(file: &std::fs::File) -> Result<()> {
    durable_sync_file_at(file, DurabilityBarrier::OtherFile)
}

#[doc(hidden)]
pub fn durable_sync_file_at(file: &std::fs::File, point: DurabilityBarrier) -> Result<()> {
    inject_barrier_failure(point)?;
    durable_sync_file_inner(file)
}

fn durable_sync_file_inner(file: &std::fs::File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if result == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(target_os = "macos"))]
    file.sync_all()?;
    Ok(())
}

pub fn durable_sync_dir(path: &Path) -> Result<()> {
    durable_sync_dir_at(path, DurabilityBarrier::OtherDirectory)
}

#[doc(hidden)]
pub fn durable_sync_dir_at(path: &Path, point: DurabilityBarrier) -> Result<()> {
    inject_barrier_failure(point)?;
    #[cfg(unix)]
    durable_sync_file_inner(&std::fs::File::open(path)?)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub struct Store {
    pub blobs: LocalBlobStore,
    pub meta: Meta,
    trees: Mutex<LruCache<ObjectId, Arc<Tree>>>,
    blob_cache: Mutex<LruCache<ObjectId, Arc<[u8]>>>,
}

pub struct StorePublishBatch<'a> {
    store: &'a Store,
    objects: Mutex<PublishBatch<'a>>,
}

impl StorePublishBatch<'_> {
    pub fn put_commit(&self, commit: &Commit) -> Result<ObjectId> {
        self.objects.lock().put(&commit.encode())
    }

    pub fn put_contribution(&self, contribution: &Contribution) -> Result<ObjectId> {
        let bytes = contribution.encode()?;
        self.objects.lock().put(&bytes)
    }

    pub fn finish(self) -> Result<()> {
        self.objects.into_inner().finish()
    }
}

impl TreeStore for StorePublishBatch<'_> {
    fn get_tree(&self, id: ObjectId) -> Result<Tree> {
        self.store.get_tree(id)
    }

    fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
        let bytes = tree.encode()?;
        let id = self.objects.lock().put(&bytes)?;
        self.store.trees.lock().put(id, Arc::new(tree.clone()));
        Ok(id)
    }
}

impl Store {
    pub fn begin_publish_batch(&self) -> StorePublishBatch<'_> {
        StorePublishBatch {
            store: self,
            objects: Mutex::new(self.blobs.begin_batch()),
        }
    }

    pub fn open(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::new(root.to_path_buf())?;
        let meta = Meta::open(&root.join("meta.sqlite"))?;
        Ok(Self {
            blobs,
            meta,
            trees: Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())),
            blob_cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),
        })
    }

    /// Open both halves of the store without writing to either. Object and
    /// metadata writes are then refused at their own boundaries, so a
    /// read-only handle cannot touch read-only media.
    pub fn open_read_only(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::open_read_only(root.to_path_buf())?;
        let meta = Meta::open_read_only(&root.join("meta.sqlite"))?;
        Ok(Self {
            blobs,
            meta,
            trees: Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())),
            blob_cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),
        })
    }

    /// Detection-only open used by fsck. It is byte-for-byte read-only like
    /// `open_read_only`, but lets the catalog auditor report a damaged schema
    /// ledger rather than rejecting it before fsck can produce a finding.
    pub fn open_read_only_for_fsck(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::open_read_only(root.to_path_buf())?;
        let meta = Meta::open_read_only_for_fsck(&root.join("meta.sqlite"))?;
        Ok(Self {
            blobs,
            meta,
            trees: Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())),
            blob_cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),
        })
    }

    pub fn read_only(&self) -> bool {
        self.meta.read_only()
    }

    pub fn put_raw(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.blobs.put(bytes)
    }

    pub fn get_raw(&self, id: ObjectId) -> Result<Vec<u8>> {
        {
            let mut c = self.blob_cache.lock();
            if let Some(b) = c.get(&id) {
                return Ok(b.to_vec());
            }
        }
        let bytes = self.get_raw_verified(id)?;
        self.blob_cache
            .lock()
            .put(id, Arc::from(bytes.clone().into_boxed_slice()));
        Ok(bytes)
    }

    /// Trust-boundary read: always hits durable bytes and re-hashes (I15).
    pub fn get_raw_verified(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.blobs.get(id)
    }

    pub fn put_blob_data(&self, data: &[u8]) -> Result<ObjectId> {
        let file = Blob {
            data: data.to_vec(),
        }
        .encode();
        self.put_raw(&file)
    }

    pub fn get_blob_data(&self, id: ObjectId) -> Result<Vec<u8>> {
        let bytes = self.get_raw(id)?;
        Ok(Blob::decode(&bytes)?.data)
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
        let bytes = tree.encode()?;
        let id = self.put_raw(&bytes)?;
        self.trees.lock().put(id, Arc::new(tree.clone()));
        Ok(id)
    }

    pub fn get_tree(&self, id: ObjectId) -> Result<Tree> {
        {
            let mut c = self.trees.lock();
            if let Some(t) = c.get(&id) {
                return Ok((**t).clone());
            }
        }
        let bytes = self.get_raw(id)?;
        let t = Tree::decode(&bytes)?;
        self.trees.lock().put(id, Arc::new(t.clone()));
        Ok(t)
    }

    pub fn put_commit(&self, c: &Commit) -> Result<ObjectId> {
        self.put_raw(&c.encode())
    }

    pub fn get_commit(&self, id: ObjectId) -> Result<Commit> {
        Commit::decode(&self.get_raw(id)?)
    }

    pub fn put_contribution(&self, contribution: &Contribution) -> Result<ObjectId> {
        self.put_raw(&contribution.encode()?)
    }

    pub fn get_contribution(&self, id: ObjectId) -> Result<Contribution> {
        Contribution::decode(&self.get_raw(id)?)
    }

    pub fn put_conflict(&self, c: &Conflict) -> Result<ObjectId> {
        self.put_raw(&c.encode())
    }

    pub fn get_conflict(&self, id: ObjectId) -> Result<Conflict> {
        Conflict::decode(&self.get_raw(id)?)
    }

    pub fn put_snapshot(&self, s: &Snapshot) -> Result<ObjectId> {
        self.put_raw(&s.encode())
    }

    pub fn get_snapshot(&self, id: ObjectId) -> Result<Snapshot> {
        Snapshot::decode(&self.get_raw(id)?)
    }

    pub fn object_type(&self, id: ObjectId) -> Result<ObjectType> {
        let b = self.get_raw(id)?;
        decode_object_type(&b)
    }

    pub fn empty_tree_id(&self) -> Result<ObjectId> {
        self.put_tree(&Tree::new(vec![])?)
    }

    pub fn root(&self) -> PathBuf {
        self.blobs.root().to_path_buf()
    }

    /// First-intro walk: record every oid in `new` that is not in `old`.
    pub fn collect_intros(&self, old: Option<ObjectId>, new: ObjectId) -> Result<Vec<ObjectId>> {
        let mut oids = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut oids)?;
        Ok(oids)
    }

    pub fn record_intros(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {
        let oids = self.collect_intros(old, new)?;
        self.meta.intro_insert_many(&oids, commit, agent)
    }

    fn intro_walk(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        expected: ObjectType,
        oids: &mut Vec<ObjectId>,
    ) -> Result<()> {
        if old == Some(new) {
            return Ok(());
        }

        let bytes = self.get_raw(new)?;
        let actual = decode_object_type(&bytes)?;
        if actual != expected {
            return Err(Error::Corrupt(format!(
                "typed edge expected {}, found {} at {new}",
                expected.as_str(),
                actual.as_str()
            )));
        }
        oids.push(new);

        match actual {
            ObjectType::Blob => {
                Blob::decode(&bytes)?;
            }
            ObjectType::Tree => {
                let new_tree = Tree::decode(&bytes)?;
                let old_tree = match old {
                    Some(id) => {
                        let old_bytes = self.get_raw(id)?;
                        match decode_object_type(&old_bytes)? {
                            ObjectType::Tree => Some(Tree::decode(&old_bytes)?),
                            other => {
                                return Err(Error::Corrupt(format!(
                                    "unexpected {} in previous tree edge at {id}",
                                    other.as_str()
                                )))
                            }
                        }
                    }
                    None => None,
                };
                let old_map = old_tree.as_ref().map(Tree::as_map).unwrap_or_default();
                for e in &new_tree.entries {
                    let old_id = old_map
                        .get(&e.name)
                        .filter(|old_e| old_e.kind == e.kind)
                        .map(|old_e| old_e.id);
                    let expected = match e.kind {
                        EntryKind::Blob => ObjectType::Blob,
                        EntryKind::Tree => ObjectType::Tree,
                    };
                    self.intro_walk(old_id, e.id, expected, oids)?;
                }
            }
            other => {
                return Err(Error::Corrupt(format!(
                    "unexpected {} in provenance tree at {new}",
                    other.as_str()
                )))
            }
        }
        Ok(())
    }

    pub fn reachable_oids(&self, tree: ObjectId) -> Result<Vec<ObjectId>> {
        self.reachable_oids_verified(tree)
    }

    /// Type-aware walk that fail-closes on decode errors (I15 / #35).
    pub fn reachable_oids_verified(&self, tree: ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self
            .reachable_graph_verified(tree, ObjectType::Tree)?
            .into_iter()
            .map(|object| object.id)
            .collect())
    }
}

impl TreeStore for Store {
    fn get_tree(&self, id: ObjectId) -> Result<Tree> {
        Store::get_tree(self, id)
    }
    fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
        Store::put_tree(self, tree)
    }
}
