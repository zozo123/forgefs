use forge_core::hash_bytes;
use forge_types::{Error, ObjectId, Result};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_TMP_MS: u64 = 24 * 60 * 60 * 1000;

/// Monotonic process-local counters for physical durability work.
/// `puts` counts newly published OIDs; dedup hits do not increment it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobStoreStats {
    pub puts: u64,
    pub fsync_file: u64,
    pub fsync_dir: u64,
}

#[derive(Debug, Default)]
struct BlobStoreCounters {
    puts: AtomicU64,
    fsync_file: AtomicU64,
    fsync_dir: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
    stats: Arc<BlobStoreCounters>,
}

/// Checkin-scoped durable publication. Every object is file-fsynced before its
/// immutable final name is linked. Final shard-directory barriers are deferred
/// until `finish`; metadata CAS is allowed only after `finish` succeeds. A crash
/// before CAS may leave durable orphan objects, which is safe.
pub struct PublishBatch<'a> {
    store: &'a LocalBlobStore,
    dirs: BTreeSet<PathBuf>,
    new_puts: u64,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        let stats = Arc::new(BlobStoreCounters::default());
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        ensure_dir_durable(&root, &objects, &stats)?;
        ensure_dir_durable(&root, &tmp, &stats)?;
        cleanup_stale_tmp(&tmp, &stats)?;
        Ok(Self { root, stats })
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
        BlobStoreStats {
            puts: self.stats.puts.load(Ordering::Relaxed),
            fsync_file: self.stats.fsync_file.load(Ordering::Relaxed),
            fsync_dir: self.stats.fsync_dir.load(Ordering::Relaxed),
        }
    }
}

impl PublishBatch<'_> {
    pub fn put(&mut self, bytes: &[u8]) -> Result<ObjectId> {
        let id = hash_bytes(bytes);
        let dest = self.store.object_path(id);
        if dest.exists() {
            self.store.verify_existing(id, &dest)?;
            return Ok(id);
        }

        let (a, b) = id.shard_dirs();
        let objects = self.store.root.join("objects");
        let shard_a = objects.join(a);
        let shard_b = shard_a.join(b);
        ensure_dir_durable(&objects, &shard_a, &self.store.stats)?;
        ensure_dir_durable(&shard_a, &shard_b, &self.store.stats)?;

        let tmp_dir = self.store.root.join("tmp");
        ensure_dir_durable(&self.store.root, &tmp_dir, &self.store.stats)?;
        let tmp = tmp_dir.join(ulid::Ulid::new().to_string());
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            self.store.stats.fsync_file.fetch_add(1, Ordering::Relaxed);
        }

        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                self.dirs.insert(shard_b);
                self.new_puts += 1;
                let _ = fs::remove_file(&tmp);
                Ok(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || dest.exists() => {
                let _ = fs::remove_file(&tmp);
                self.store.verify_existing(id, &dest)?;
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

fn ensure_dir_durable(parent: &Path, child: &Path, stats: &BlobStoreCounters) -> Result<()> {
    match fs::create_dir(child) {
        Ok(()) => sync_dir_counted(parent, stats),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(Error::Io(e.to_string())),
    }
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
    fs::File::open(path)?.sync_all()?;
    stats.fsync_dir.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(s.stats(), after);
        assert_eq!(s.get(id).unwrap(), b"measured");
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
    fn corrupt_existing_object_is_rejected() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let id = s.put(b"good").unwrap();
        fs::write(s.object_path(id), b"evil").unwrap();
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
        assert!(matches!(s.put(b"good"), Err(Error::Corrupt(_))));
    }
}
