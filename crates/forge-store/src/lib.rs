//! Local content-addressed objects + SQLite mutable surface.

pub mod blob;
mod graph;
pub mod meta;
mod metrics;
pub mod objectstore;

pub use blob::{
    BlobStoreStats, DirectoryBarrier, GcObjectGuard, LocalBlobStore, PublishBatch, StagedBlobReader,
};
pub use graph::{
    decode_graph_object, DecodedGraphObject, GraphEdge, GraphExpectation, GraphWorkQueue,
    VerifiedGraphObject, DEFAULT_MAX_GRAPH_OBJECTS, MAX_GRAPH_OBJECTS_ENV,
};
pub use meta::{
    sanitize_agent, validate_ref_kind, validate_ref_name, AbandonedSession, CatalogAudit,
    CatalogObjectExpectation, CheckpointResult, DurabilityPolicy, GcCatalogRoots, GcSweepTxn,
    LedgerStanding, Meta, MetaStats, MountRow, NsRow, Observed, OverlayRow, RetiredRef,
    ABANDONABLE_PREFIX, CURRENT_SCHEMA_VERSION, REFLOG_ABANDON,
};
pub use objectstore::{DurabilityClass, ObjectBatch, ObjectStore};

use forge_core::object::{decode_object_type, Blob, Commit, Conflict, Snapshot};
use forge_core::tree::{Tree, TreeStore};
use forge_core::Contribution;
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

const OBJECT_CACHE_MAX_ENTRIES: usize = 256;

/// Maximum encoded object bytes retained by one `Store`'s hot raw-object cache.
///
/// The cache remains capped at 256 entries as well. The 64 MiB byte ceiling is
/// the same measured threshold at which the CLI already warns about a large
/// blob (docs/CHUNKING.md). An individual object larger than the budget is read
/// normally and simply not cached. This is memory policy only: trust-boundary
/// reads still bypass the cache and re-hash durable bytes (I15).
pub const OBJECT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

struct ObjectCache {
    entries: LruCache<ObjectId, Arc<[u8]>>,
    bytes: usize,
}

impl ObjectCache {
    fn new() -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(OBJECT_CACHE_MAX_ENTRIES).unwrap()),
            bytes: 0,
        }
    }

    fn get(&mut self, id: &ObjectId) -> Option<&Arc<[u8]>> {
        self.entries.get(id)
    }

    fn put(&mut self, id: ObjectId, value: Arc<[u8]>) {
        let incoming = value.len();
        if incoming > OBJECT_CACHE_MAX_BYTES {
            self.pop(&id);
            return;
        }

        self.bytes += incoming;
        if let Some((_id, old)) = self.entries.push(id, value) {
            self.bytes -= old.len();
        }
        while self.bytes > OBJECT_CACHE_MAX_BYTES {
            let Some((_id, old)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= old.len();
        }
        debug_assert!(self.bytes <= OBJECT_CACHE_MAX_BYTES);
    }

    fn pop(&mut self, id: &ObjectId) {
        if let Some(old) = self.entries.pop(id) {
            self.bytes -= old.len();
        }
    }
}

/// The object plane is a type parameter, not a file layout. `Store` is written
/// against [`ObjectStore`] and defaults to the only production implementation,
/// so the bare name `Store` still means `Store<LocalBlobStore>` everywhere it
/// did before -- while the compiler now proves this type uses nothing but the
/// trait.
pub struct Store<O: ObjectStore = LocalBlobStore> {
    pub blobs: O,
    pub meta: Meta,
    /// The repository root. It belongs to the repository, not to the object
    /// plane: `meta.sqlite`, `VERSION` and `keys/` live here whatever backend
    /// holds the objects.
    root: PathBuf,
    trees: Mutex<LruCache<ObjectId, Arc<Tree>>>,
    blob_cache: Mutex<ObjectCache>,
    cache: CacheCounters,
}

/// Hit and miss counts for the two hot LRU caches a `Store` keeps.
///
/// Evidence, never policy: nothing here decides whether a read is served from
/// memory. They exist because `BlobStoreStats::get_bytes` is PHYSICAL read
/// volume, so without them a fall in physical reads is indistinguishable from
/// a fall in work. A miss is counted where the lookup failed, so
/// `hits + misses` is the number of lookups and never the number of objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreCacheStats {
    pub object_cache_hits: u64,
    pub object_cache_misses: u64,
    pub tree_cache_hits: u64,
    pub tree_cache_misses: u64,
}

#[derive(Debug, Default)]
struct CacheCounters {
    object_hits: AtomicU64,
    object_misses: AtomicU64,
    tree_hits: AtomicU64,
    tree_misses: AtomicU64,
}

pub struct StorePublishBatch<'a, O: ObjectStore = LocalBlobStore> {
    store: &'a Store<O>,
    objects: Mutex<Box<dyn ObjectBatch + 'a>>,
}

impl<O: ObjectStore> StorePublishBatch<'_, O> {
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

impl<O: ObjectStore> TreeStore for StorePublishBatch<'_, O> {
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

impl<O: ObjectStore> Store<O> {
    /// Build a `Store` over any object plane. This is the seam's entry point:
    /// the catalog stays local while the caller chooses the object backend.
    /// Only a [`DurabilityClass::CrashDurable`] backend may back a repository
    /// that publishes refs -- see `objectstore.rs` for why that is a review
    /// obligation and not a compile error.
    pub fn with_object_store(root: PathBuf, blobs: O, meta: Meta) -> Self {
        Self {
            blobs,
            meta,
            root,
            trees: Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())),
            blob_cache: Mutex::new(ObjectCache::new()),
            cache: CacheCounters::default(),
        }
    }

    /// Snapshot the cache counters. Relaxed loads, taken independently: a
    /// diagnostic read, never consistent to a single instant.
    pub fn cache_stats(&self) -> StoreCacheStats {
        StoreCacheStats {
            object_cache_hits: self.cache.object_hits.load(Ordering::Relaxed),
            object_cache_misses: self.cache.object_misses.load(Ordering::Relaxed),
            tree_cache_hits: self.cache.tree_hits.load(Ordering::Relaxed),
            tree_cache_misses: self.cache.tree_misses.load(Ordering::Relaxed),
        }
    }

    pub fn begin_publish_batch(&self) -> StorePublishBatch<'_, O> {
        StorePublishBatch {
            store: self,
            objects: Mutex::new(self.blobs.begin_batch()),
        }
    }
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::new(root.to_path_buf())?;
        let meta = Meta::open(&root.join("meta.sqlite"))?;
        Ok(Self::with_object_store(root.to_path_buf(), blobs, meta))
    }

    /// Open both halves of the store without writing to either. Object and
    /// metadata writes are then refused at their own boundaries, so a
    /// read-only handle cannot touch read-only media.
    pub fn open_read_only(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::open_read_only(root.to_path_buf())?;
        let meta = Meta::open_read_only(&root.join("meta.sqlite"))?;
        Ok(Self::with_object_store(root.to_path_buf(), blobs, meta))
    }

    /// Local staged-output adapter. Bytes read from the returned handle are not
    /// trusted until `finish()` succeeds; therefore this API is intentionally
    /// unavailable on generic `Store<O>` and must never back stdout directly.
    pub fn open_blob_payload_for_staged_output(&self, id: ObjectId) -> Result<StagedBlobReader> {
        self.blobs.open_blob_payload_for_staged_output(id)
    }

    /// Detection-only open used by fsck. It is byte-for-byte read-only like
    /// `open_read_only`, but lets the catalog auditor report a damaged schema
    /// ledger rather than rejecting it before fsck can produce a finding.
    /// Take the object plane's reclamation lock exclusively for the lifetime
    /// of the guard, so a sweep's age check and its unlink are one decision.
    ///
    /// Local-backend only, like the object enumeration `gc` and `fsck` already
    /// do directly; `objectstore.rs` names that surface rather than pretending
    /// the trait covers it.
    pub fn gc_exclusive_objects(&self) -> Result<GcObjectGuard> {
        self.blobs.gc_exclusive()
    }

    pub fn open_read_only_for_fsck(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::open_read_only(root.to_path_buf())?;
        let meta = Meta::open_read_only_for_fsck(&root.join("meta.sqlite"))?;
        Ok(Self::with_object_store(root.to_path_buf(), blobs, meta))
    }
}

impl<O: ObjectStore> Store<O> {
    pub fn read_only(&self) -> bool {
        self.meta.read_only()
    }

    pub fn put_raw(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.blobs.put(bytes)
    }

    /// Publish the object file formed by concatenating `parts`. Same bytes,
    /// same ObjectId and same barriers as `put_raw(&parts.concat())`.
    pub fn put_raw_parts(&self, parts: &[&[u8]]) -> Result<ObjectId> {
        self.blobs.put_parts(parts)
    }

    pub fn get_raw(&self, id: ObjectId) -> Result<Vec<u8>> {
        {
            let mut c = self.blob_cache.lock();
            if let Some(b) = c.get(&id) {
                self.cache.object_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(b.to_vec());
            }
        }
        self.cache.object_misses.fetch_add(1, Ordering::Relaxed);
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

    /// Drop every cached copy of `id`.
    ///
    /// The hot LRU caches assume immutability, which is true of an object's
    /// bytes and false of its *existence*: after a collector unlinks an
    /// object, `get_raw` in the collecting process would keep serving it from
    /// memory, which hides exactly the bug a collector must not have. The
    /// sweep calls this on every object it unlinks so absence is observable in
    /// the process that caused it, not only after a cold reopen (I23).
    pub fn forget_cached(&self, id: ObjectId) {
        self.trees.lock().pop(&id);
        self.blob_cache.lock().pop(&id);
    }

    /// Publish `data` as a Blob without copying it. The published bytes are
    /// identical to `Blob { data }.encode()`; what is gone is the pair of
    /// full-payload allocations that shape used (`to_vec`, then the encode
    /// buffer), so publishing costs the caller's buffer plus a 16-byte frame
    /// instead of three times the payload. Identity is unchanged (I2), and so
    /// is the VERSION 1 encoding (FORMAT.md).
    pub fn put_blob_data(&self, data: &[u8]) -> Result<ObjectId> {
        let prefix = forge_core::blob_frame_prefix(data.len() as u64);
        self.put_raw_parts(&[&prefix, data])
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
                self.cache.tree_hits.fetch_add(1, Ordering::Relaxed);
                return Ok((**t).clone());
            }
        }
        self.cache.tree_misses.fetch_add(1, Ordering::Relaxed);
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
        // #355: an over-large conflict is a merge a caller asked for, not a
        // damaged object. `Conflict::encode` cannot fail, so this is the one
        // gate between caller input and bytes `Conflict::decode` would later
        // reject as `Corrupt`.
        c.validate()?;
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
        self.root.clone()
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

        if expected == ObjectType::Blob {
            self.blobs.verify_blob(new)?;
            oids.push(new);
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

impl<O: ObjectStore> TreeStore for Store<O> {
    fn get_tree(&self, id: ObjectId) -> Result<Tree> {
        Store::<O>::get_tree(self, id)
    }
    fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
        Store::<O>::put_tree(self, tree)
    }
}
