//! A second `ObjectStore` implementation that exists only to keep the first one
//! honest.
//!
//! It is `#[cfg(test)]`-only: it cannot be linked into a release build, and
//! [`DurabilityClass::ProcessLifetime`] states in the API what that means. Its
//! job is to model the *contract* -- two-phase publication, the join rule,
//! barrier accounting, content-addressed verification -- with no filesystem at
//! all, so that anything the conformance suite asserts is provably a property
//! of the seam rather than of POSIX.

use super::{DurabilityClass, ObjectBatch, ObjectStore};
use crate::blob::BlobStoreStats;
use forge_core::{hash_bytes, hash_parts};
use forge_types::{Error, ObjectId, Result};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Inner {
    /// Readable right now. Visibility, explicitly not durability.
    visible: BTreeMap<ObjectId, Vec<u8>>,
    /// Proven by a `finish` that returned `Ok`. Nothing else may add to this.
    durable: BTreeSet<ObjectId>,
    stats: BlobStoreStats,
}

pub(crate) struct MemoryObjectStore {
    inner: Mutex<Inner>,
    read_only: bool,
}

impl MemoryObjectStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            read_only: false,
        }
    }

    pub(crate) fn new_read_only() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            read_only: true,
        }
    }

    /// Test-only tamper hook, the in-memory analogue of overwriting an object
    /// file. It exists so the conformance suite can prove that `get` re-hashes
    /// instead of trusting its own index.
    pub(crate) fn corrupt(&self, id: ObjectId, bytes: &[u8]) {
        self.inner.lock().visible.insert(id, bytes.to_vec());
    }

    fn is_durable(&self, id: ObjectId) -> bool {
        self.inner.lock().durable.contains(&id)
    }
}

pub(crate) struct MemoryBatch<'a> {
    store: &'a MemoryObjectStore,
    /// OIDs whose naming barrier this batch still owes at `finish`.
    owed: BTreeSet<ObjectId>,
    new_puts: u64,
    new_dedup: u64,
}

impl ObjectBatch for MemoryBatch<'_> {
    /// The model has nowhere to write vectored -- it holds one `Vec<u8>` per
    /// object -- so it concatenates in the open, which is exactly what the
    /// trait requires of an implementation that cannot gather. The address is
    /// still taken over the parts with `hash_parts`, never over the buffer it
    /// happens to build, so a backend-visible disagreement between the two
    /// would fail here and not only in the local store.
    fn put_parts(&mut self, parts: &[&[u8]]) -> Result<ObjectId> {
        if self.store.read_only {
            return Err(Error::Denied(
                "repository is open read-only; objects cannot be published".into(),
            ));
        }
        let id = hash_parts(parts);
        let mut inner = self.store.inner.lock();
        if let Some(existing) = inner.visible.get(&id) {
            if hash_bytes(existing) != id {
                return Err(Error::Corrupt(format!(
                    "existing object does not match its id: {id}"
                )));
            }
            self.new_dedup += 1;
            if inner.durable.contains(&id) {
                // Already proven by a finished batch; no new barrier.
                return Ok(id);
            }
            // Visible but unproven: some other batch made it readable and has
            // not finished. Inheriting its visibility as durability is exactly
            // the I4 violation this seam exists to prevent, so pay the object
            // barrier now and owe the naming barrier at finish.
            inner.stats.fsync_file += 1;
            inner.stats.fsync_file_us += 1;
            self.owed.insert(id);
            return Ok(id);
        }
        inner.visible.insert(id, parts.concat());
        inner.stats.fsync_file += 1;
        inner.stats.fsync_file_us += 1;
        self.owed.insert(id);
        self.new_puts += 1;
        Ok(id)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let mut inner = self.store.inner.lock();
        if !self.owed.is_empty() {
            inner.stats.fsync_dir += 1;
            inner.stats.fsync_dir_us += 1;
        }
        for id in &self.owed {
            inner.durable.insert(*id);
        }
        inner.stats.puts += self.new_puts;
        inner.stats.dedup_hits += self.new_dedup;
        Ok(())
    }
}

impl ObjectStore for MemoryObjectStore {
    fn durability_class(&self) -> DurabilityClass {
        DurabilityClass::ProcessLifetime
    }

    fn begin_batch(&self) -> Box<dyn ObjectBatch + '_> {
        Box::new(MemoryBatch {
            store: self,
            owed: BTreeSet::new(),
            new_puts: 0,
            new_dedup: 0,
        })
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        let inner = self.inner.lock();
        let bytes = inner
            .visible
            .get(&id)
            .ok_or_else(|| Error::NotFound(format!("object {id}")))?;
        if hash_bytes(bytes) != id {
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        Ok(bytes.clone())
    }

    fn has(&self, id: ObjectId) -> bool {
        self.inner.lock().visible.contains_key(&id)
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn stats(&self) -> BlobStoreStats {
        self.inner.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model must not be able to cheat the property the whole seam rests
    /// on: only a `finish` that returned `Ok` may create a durability proof.
    #[test]
    fn dropping_a_batch_leaves_the_object_visible_but_unproven() {
        let s = MemoryObjectStore::new();
        let mut batch = s.begin_batch();
        let id = batch.put(b"unproven").unwrap();
        drop(batch);
        assert!(s.has(id), "the bytes are readable");
        assert!(!s.is_durable(id), "but nothing proved the name");
        assert_eq!(s.stats().puts, 0, "a dropped batch publishes no accounting");
    }
}
