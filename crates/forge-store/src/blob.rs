use forge_core::hash_bytes;
use forge_types::{Error, ObjectId, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_TMP_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        ensure_dir_durable(&root, &objects)?;
        ensure_dir_durable(&root, &tmp)?;
        cleanup_stale_tmp(&tmp)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, id: ObjectId) -> PathBuf {
        let (a, b) = id.shard_dirs();
        self.root.join("objects").join(a).join(b).join(id.hex())
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        let id = hash_bytes(bytes);
        let dest = self.object_path(id);
        if dest.exists() {
            self.verify_existing(id, &dest)?;
            return Ok(id);
        }

        // Create each shard one level at a time and fsync the parent that owns
        // its new directory entry. Otherwise a power loss can retain the file
        // fsync while losing a newly created shard path.
        let (a, b) = id.shard_dirs();
        let objects = self.root.join("objects");
        let shard_a = objects.join(a);
        let shard_b = shard_a.join(b);
        ensure_dir_durable(&objects, &shard_a)?;
        ensure_dir_durable(&shard_a, &shard_b)?;

        let tmp_dir = self.root.join("tmp");
        ensure_dir_durable(&self.root, &tmp_dir)?;
        let tmp = tmp_dir.join(ulid::Ulid::new().to_string());

        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            // The bytes themselves must reach stable storage before publication.
            f.sync_all()?;
        }

        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                // Persist the final directory entry before metadata can publish
                // the OID. At this point every shard directory is durable too.
                sync_dir(&shard_b)?;
                let _ = fs::remove_file(&tmp);
                Ok(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || dest.exists() => {
                let _ = fs::remove_file(&tmp);
                // A same-name object is only acceptable if its bytes really
                // hash to the requested OID. This catches disk corruption and
                // accidental out-of-band modification instead of blessing it.
                self.verify_existing(id, &dest)?;
                Ok(id)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(Error::Io(e.to_string()))
            }
        }
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
}

fn ensure_dir_durable(parent: &Path, child: &Path) -> Result<()> {
    match fs::create_dir(child) {
        Ok(()) => sync_dir(parent),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(Error::Io(e.to_string())),
    }
}

/// Reclaim only crash debris old enough that it cannot plausibly be an active
/// publication. Every Forge temp file is named by its creation-time ULID.
/// Eagerly deleting all temp files on every Store::open races other processes.
fn cleanup_stale_tmp(tmp: &Path) -> Result<()> {
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
        sync_dir(tmp)?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
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
    fn corrupt_existing_object_is_rejected() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let id = s.put(b"good").unwrap();
        fs::write(s.object_path(id), b"evil").unwrap();
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
        assert!(matches!(s.put(b"good"), Err(Error::Corrupt(_))));
    }
}
