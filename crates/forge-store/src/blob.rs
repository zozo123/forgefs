use forge_core::hash_bytes;
use forge_types::{Error, ObjectId, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("tmp"))?;
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

        let parent = dest
            .parent()
            .ok_or_else(|| Error::Internal("object path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        fs::create_dir_all(self.root.join("tmp"))?;
        let tmp = self.root.join("tmp").join(ulid::Ulid::new().to_string());

        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            // The bytes themselves must reach stable storage before publication.
            f.sync_all()?;
        }

        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                // Persist the new directory entry before we allow metadata to
                // publish this OID. A file fsync alone is not sufficient for
                // crash durability of the name.
                sync_dir(parent)?;
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

fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn leftover_tmp_ignored() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        fs::write(d.path().join("tmp").join("junk"), b"nope").unwrap();
        let id = s.put(b"ok").unwrap();
        assert_eq!(s.get(id).unwrap(), b"ok");
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
