use crate::metrics::TimingCounter;
use forge_core::hash_bytes;
use forge_types::{Error, ObjectId, Result};
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const STALE_TMP_MS: u64 = 24 * 60 * 60 * 1000;
const DURABLE_DIR_CACHE_CAPACITY: usize = 65_536;
const DURABLE_OID_CACHE_CAPACITY: usize = 65_536;

/// Monotonic process-local counters for physical durability work.
/// `puts` counts newly published OIDs; dedup hits do not increment it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobStoreStats {
    pub puts: u64,
    /// Successful file durability barriers.
    pub fsync_file: u64,
    /// Cumulative elapsed time for successful file durability barriers.
    pub fsync_file_us: u64,
    /// Successful directory durability barriers.
    pub fsync_dir: u64,
    /// Cumulative elapsed time for successful directory durability barriers.
    pub fsync_dir_us: u64,
}

impl BlobStoreStats {
    pub fn barrier_us(&self) -> u64 {
        self.fsync_file_us.saturating_add(self.fsync_dir_us)
    }
}

#[derive(Debug, Default)]
struct BlobStoreCounters {
    puts: AtomicU64,
    fsync_file: TimingCounter,
    fsync_dir: TimingCounter,
}

#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
    stats: Arc<BlobStoreCounters>,
    // Positive durability proofs only. A directory is inserted after its
    // parent barrier succeeds; an OID is inserted only after the batch's leaf
    // barrier succeeds. Both caches are deliberately cold on every Store open
    // so a new process re-proves state left visible by a crashed peer.
    durable_dirs: Arc<Mutex<LruCache<PathBuf, ()>>>,
    durable_oids: Arc<Mutex<LruCache<ObjectId, ()>>>,
}

/// Checkin-scoped durable publication. Every object is file-fsynced before its
/// immutable final name is linked. Final shard-directory barriers are deferred
/// until `finish`; metadata CAS is allowed only after `finish` succeeds. A crash
/// before CAS may leave durable orphan objects, which is safe.
pub struct PublishBatch<'a> {
    store: &'a LocalBlobStore,
    dirs: BTreeSet<PathBuf>,
    oids: BTreeSet<ObjectId>,
    new_puts: u64,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        let stats = Arc::new(BlobStoreCounters::default());
        let durable_dirs = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DURABLE_DIR_CACHE_CAPACITY).expect("non-zero directory cache"),
        )));
        let durable_oids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DURABLE_OID_CACHE_CAPACITY).expect("non-zero OID cache"),
        )));
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        ensure_dir_durable(&root, &objects, &stats, &durable_dirs)?;
        ensure_dir_durable(&root, &tmp, &stats, &durable_dirs)?;
        cleanup_stale_tmp(&tmp, &stats)?;
        Ok(Self {
            root,
            stats,
            durable_dirs,
            durable_oids,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, id: ObjectId) -> PathBuf {
        let (a, b) = id.shard_dirs();
        self.root.join("objects").join(a).join(b).join(id.hex())
    }

    pub fn begin_batch(&self) -> PublishBatch<'_> {
        PublishBatch {
            store: self,
            dirs: BTreeSet::new(),
            oids: BTreeSet::new(),
            new_puts: 0,
        }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        let mut batch = self.begin_batch();
        let id = batch.put(bytes)?;
        batch.finish()?;
        Ok(id)
    }

    fn verify_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        let bytes = fs::read(path).map_err(|e| Error::Io(e.to_string()))?;
        if hash_bytes(&bytes) != id {
            return Err(Error::Corrupt(format!(
                "existing object does not match its id: {id}"
            )));
        }
        Ok(())
    }

    fn verify_and_sync_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        // Operate on one descriptor so the bytes verified are the bytes forced.
        // Opening writable is intentional: macOS F_FULLFSYNC is a fail-closed
        // durability contract, not a best-effort read hint.
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if hash_bytes(&bytes) != id {
            return Err(Error::Corrupt(format!(
                "existing object does not match its id: {id}"
            )));
        }
        sync_file_counted(&file, &self.stats)?;
        Ok(())
    }

    fn oid_is_durable(&self, id: ObjectId) -> bool {
        self.durable_oids.lock().get(&id).is_some()
    }

    pub fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        let p = self.object_path(id);
        let bytes = fs::read(&p).map_err(|_| Error::NotFound(format!("object {id}")))?;
        if hash_bytes(&bytes) != id {
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        Ok(bytes)
    }

    pub fn has(&self, id: ObjectId) -> bool {
        self.object_path(id).exists()
    }

    pub fn stats(&self) -> BlobStoreStats {
        let fsync_file = self.stats.fsync_file.snapshot();
        let fsync_dir = self.stats.fsync_dir.snapshot();
        BlobStoreStats {
            puts: self.stats.puts.load(Ordering::Relaxed),
            fsync_file: fsync_file.count,
            fsync_file_us: fsync_file.total_us,
            fsync_dir: fsync_dir.count,
            fsync_dir_us: fsync_dir.total_us,
        }
    }
}

impl PublishBatch<'_> {
    pub fn put(&mut self, bytes: &[u8]) -> Result<ObjectId> {
        let id = hash_bytes(bytes);
        let dest = self.store.object_path(id);
        let (a, b) = id.shard_dirs();
        let objects = self.store.root.join("objects");
        let shard_a = objects.join(a);
        let shard_b = shard_a.join(b);

        // Prove the complete pathname before trusting either an existing link
        // or a link we are about to publish. A cold Store cannot inherit a
        // crashed or older VERSION=1 process's unforced shard ancestors.
        ensure_dir_durable(
            &objects,
            &shard_a,
            &self.store.stats,
            &self.store.durable_dirs,
        )?;
        ensure_dir_durable(
            &shard_a,
            &shard_b,
            &self.store.stats,
            &self.store.durable_dirs,
        )?;

        if dest.exists() {
            if self.store.oid_is_durable(id) {
                // Rehash at every trust boundary even when a process-local
                // durability proof lets us avoid another physical barrier.
                self.store.verify_existing(id, &dest)?;
                return Ok(id);
            }
            self.store.verify_and_sync_existing(id, &dest)?;
            // The visible link may have been published by another batch that
            // has not yet synced this directory, or by a legacy process whose
            // file/ancestor barriers were weaker. Reproduce the entire proof
            // before allowing our caller to publish metadata that names it.
            self.dirs.insert(shard_b);
            self.oids.insert(id);
            return Ok(id);
        }

        let tmp_dir = self.store.root.join("tmp");
        let tmp = tmp_dir.join(ulid::Ulid::new().to_string());
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            sync_file_counted(&f, &self.store.stats)?;
        }

        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                self.dirs.insert(shard_b);
                self.oids.insert(id);
                self.new_puts += 1;
                let _ = fs::remove_file(&tmp);
                Ok(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || dest.exists() => {
                let _ = fs::remove_file(&tmp);
                if !self.store.oid_is_durable(id) {
                    self.store.verify_and_sync_existing(id, &dest)?;
                    self.dirs.insert(shard_b);
                    self.oids.insert(id);
                } else {
                    self.store.verify_existing(id, &dest)?;
                }
                // Another publisher won after our existence check. Its file
                // and directory barriers may still be pending, so this batch
                // joins the proof unless another completed batch cached it.
                Ok(id)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(Error::Io(e.to_string()))
            }
        }
    }

    pub fn finish(self) -> Result<()> {
        for dir in &self.dirs {
            sync_dir_counted(dir, &self.store.stats)?;
        }
        // This is the sole OID-proof publication point. A dropped or failed
        // batch never teaches later callers that its visible links are durable.
        {
            let mut durable_oids = self.store.durable_oids.lock();
            for id in self.oids {
                durable_oids.put(id, ());
            }
        }
        self.store
            .stats
            .puts
            .fetch_add(self.new_puts, Ordering::Relaxed);
        Ok(())
    }
}

impl crate::Store {
    pub fn stats(&self) -> BlobStoreStats {
        self.blobs.stats()
    }
}

fn ensure_dir_durable(
    parent: &Path,
    child: &Path,
    stats: &BlobStoreCounters,
    durable_dirs: &Mutex<LruCache<PathBuf, ()>>,
) -> Result<()> {
    if durable_dirs.lock().get(child).is_some() {
        return Ok(());
    }
    match fs::create_dir(child) {
        Ok(()) => {}
        // Existence is not proof that another process durably published the
        // directory entry. Reproduce the parent barrier before depending on it.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(child)?;
            if !metadata.file_type().is_dir() {
                return Err(Error::Invalid(format!(
                    "object-store path is not a directory: {}",
                    child.display()
                )));
            }
        }
        Err(e) => return Err(Error::Io(e.to_string())),
    }
    sync_dir_counted(parent, stats)?;
    durable_dirs.lock().put(child.to_path_buf(), ());
    Ok(())
}

/// Reclaim only crash debris old enough that it cannot plausibly be an active
/// publication. Every Forge temp file is named by its creation-time ULID.
/// Eagerly deleting all temp files on every Store::open races other processes.
fn cleanup_stale_tmp(tmp: &Path, stats: &BlobStoreCounters) -> Result<()> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Internal(format!("clock before unix epoch: {e}")))?
        .as_millis() as u64;
    let mut removed = false;
    for entry in fs::read_dir(tmp)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if !(ty.is_file() || ty.is_symlink()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = ulid::Ulid::from_string(&name) else {
            continue;
        };
        if now_ms.saturating_sub(id.timestamp_ms()) < STALE_TMP_MS {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    if removed {
        sync_dir_counted(tmp, stats)?;
    }
    Ok(())
}

fn sync_dir_counted(path: &Path, stats: &BlobStoreCounters) -> Result<()> {
    let started = Instant::now();
    crate::durable_sync_dir(path)?;
    stats.fsync_dir.observe(started.elapsed());
    Ok(())
}

fn sync_file_counted(file: &std::fs::File, stats: &BlobStoreCounters) -> Result<()> {
    let started = Instant::now();
    crate::durable_sync_file(file)?;
    stats.fsync_file.observe(started.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn put_get_idempotent() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let a = s.put(b"abc").unwrap();
        let b = s.put(b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.get(a).unwrap(), b"abc");
        assert_eq!(
            std::fs::read_dir(d.path().join("objects")).unwrap().count(),
            1
        );
    }

    #[test]
    fn startup_reclaims_old_tmp_but_never_fresh_tmp() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("tmp")).unwrap();
        let stale = d.path().join("tmp").join(ulid::Ulid::nil().to_string());
        let fresh = d.path().join("tmp").join(ulid::Ulid::new().to_string());
        fs::write(&stale, b"crash debris").unwrap();
        fs::write(&fresh, b"live publication").unwrap();

        let _s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn concurrent_same_object_writers_converge() {
        let d = tempdir().unwrap();
        let s = Arc::new(LocalBlobStore::new(d.path().to_path_buf()).unwrap());
        let mut joins = Vec::new();
        for _ in 0..32 {
            let s = s.clone();
            joins.push(thread::spawn(move || {
                s.put(b"same durable object").unwrap()
            }));
        }
        let ids: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(s.get(ids[0]).unwrap(), b"same durable object");
    }

    #[test]
    fn published_object_survives_store_reopen() {
        let d = tempdir().unwrap();
        let id = {
            let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
            s.put(b"durable").unwrap()
        };
        let reopened = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(id).unwrap(), b"durable");
    }

    #[test]
    fn stats_count_new_durable_publications_not_dedup_hits() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let before = s.stats();
        let id = s.put(b"measured").unwrap();
        let after = s.stats();
        assert_eq!(after.puts, before.puts + 1);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert!(after.fsync_dir > before.fsync_dir);
        s.put(b"measured").unwrap();
        let after_dedup = s.stats();
        assert_eq!(after_dedup, after);
        assert_eq!(s.get(id).unwrap(), b"measured");
    }

    #[test]
    fn existing_directory_reproduces_a_possible_crashed_creators_parent_barrier() {
        let d = tempdir().unwrap();
        let parent = d.path().join("parent");
        let child = parent.join("visible-but-not-proven-durable");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&child).unwrap();
        let stats = BlobStoreCounters::default();
        let durable_dirs = Mutex::new(LruCache::new(NonZeroUsize::new(8).unwrap()));

        ensure_dir_durable(&parent, &child, &stats, &durable_dirs).unwrap();

        assert_eq!(stats.fsync_dir.snapshot().count, 1);
        ensure_dir_durable(&parent, &child, &stats, &durable_dirs).unwrap();
        assert_eq!(
            stats.fsync_dir.snapshot().count,
            1,
            "a successful positive proof is reusable within one Store"
        );
    }

    #[test]
    fn dedup_batch_reproduces_an_unfinished_publishers_directory_barrier() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let mut first = s.begin_batch();
        let id = first.put(b"race-safe durable object").unwrap();
        let before = s.stats();

        let mut second = s.begin_batch();
        assert_eq!(second.put(b"race-safe durable object").unwrap(), id);
        let joined = s.stats();
        assert_eq!(joined.puts, before.puts);
        assert_eq!(joined.fsync_file, before.fsync_file + 1);
        assert_eq!(
            joined.fsync_dir, before.fsync_dir,
            "the pathname barrier is deferred to batch finish"
        );

        // Model the winning publisher dying after link(2) but before finish.
        // The deduplicating batch must still make the shared directory entry
        // durable before its caller is allowed to publish a ref.
        drop(first);
        second.finish().unwrap();
        let after = s.stats();
        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 1);
        assert_eq!(s.get(id).unwrap(), b"race-safe durable object");
    }

    #[test]
    fn cold_store_dedup_reproves_file_and_full_path() {
        let d = tempdir().unwrap();
        let first = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let mut unfinished = first.begin_batch();
        let id = unfinished.put(b"cross-process durability join").unwrap();

        // A newly opened Store has no inherited proof cache. Model a second
        // process joining the visible link after the first dies before finish.
        let second = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let before = second.stats();
        let mut joining = second.begin_batch();
        assert_eq!(joining.put(b"cross-process durability join").unwrap(), id);
        drop(unfinished);
        joining.finish().unwrap();
        let after = second.stats();

        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(
            after.fsync_dir,
            before.fsync_dir + 3,
            "objects->aa, aa->bb, and bb->OID must all be re-proved"
        );
    }

    fn same_shard_pair() -> (Vec<u8>, Vec<u8>) {
        let mut seen = std::collections::BTreeMap::new();
        for i in 0..200_000u64 {
            let bytes = format!("same-shard-{i}").into_bytes();
            let id = hash_bytes(&bytes);
            let shard = id.shard_dirs();
            if let Some(first) = seen.insert(shard, bytes.clone()) {
                if first != bytes {
                    return (first, bytes);
                }
            }
        }
        panic!("failed to find deterministic shard collision");
    }

    #[test]
    fn batch_coalesces_same_shard_directory_barrier() {
        let d = tempdir().unwrap();
        let (a, b) = same_shard_pair();

        let serial_root = d.path().join("serial");
        std::fs::create_dir(&serial_root).unwrap();
        let serial = LocalBlobStore::new(serial_root).unwrap();
        let serial_before = serial.stats();
        serial.put(&a).unwrap();
        serial.put(&b).unwrap();
        let serial_after = serial.stats();
        let serial_dirs = serial_after.fsync_dir - serial_before.fsync_dir;

        let batched_root = d.path().join("batched");
        std::fs::create_dir(&batched_root).unwrap();
        let batched = LocalBlobStore::new(batched_root).unwrap();
        let batch_before = batched.stats();
        let mut batch = batched.begin_batch();
        batch.put(&a).unwrap();
        batch.put(&b).unwrap();
        let mid = batched.stats();
        assert_eq!(mid.puts, batch_before.puts);
        batch.finish().unwrap();
        let batch_after = batched.stats();
        let batch_dirs = batch_after.fsync_dir - batch_before.fsync_dir;

        assert_eq!(batch_after.puts - batch_before.puts, 2);
        assert_eq!(batch_after.fsync_file - batch_before.fsync_file, 2);
        assert_eq!(serial_dirs, batch_dirs + 1);
    }

    #[test]
    fn warm_same_shard_put_pays_only_the_new_leaf_barrier() {
        let d = tempdir().unwrap();
        let (a, b) = same_shard_pair();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        s.put(&a).unwrap();
        let before = s.stats();

        s.put(&b).unwrap();
        let after = s.stats();

        assert_eq!(after.puts, before.puts + 1);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 1);
    }

    #[test]
    fn legacy_visible_object_reproves_uncached_ancestors() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let bytes = b"legacy visible object";
        let id = hash_bytes(bytes);
        let dest = s.object_path(id);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, bytes).unwrap();
        let before = s.stats();

        assert_eq!(s.put(bytes).unwrap(), id);
        let after = s.stats();

        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 3);
    }

    #[test]
    fn corrupt_existing_object_is_rejected() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let id = s.put(b"good").unwrap();
        fs::write(s.object_path(id), b"evil").unwrap();
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
        assert!(matches!(s.put(b"good"), Err(Error::Corrupt(_))));
    }
}
