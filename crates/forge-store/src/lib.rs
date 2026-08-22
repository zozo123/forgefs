//! Local content-addressed objects + SQLite mutable surface.

pub mod blob;
pub mod meta;

pub use blob::LocalBlobStore;
pub use meta::{sanitize_agent, Meta, MountRow, NsRow, OverlayRow};

use forge_core::object::{decode_object_type, Blob, Commit, Conflict, Snapshot};
use forge_core::tree::{Tree, TreeStore};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Store {
    pub blobs: LocalBlobStore,
    pub meta: Meta,
    trees: Mutex<LruCache<ObjectId, Arc<Tree>>>,
    blob_cache: Mutex<LruCache<ObjectId, Arc<[u8]>>>,
}

impl Store {
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
    pub fn record_intros(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {
        let mut oids = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut oids)?;
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
                            ObjectType::Blob => None,
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
        let mut out = Vec::new();
        let mut stack = vec![(tree, ObjectType::Tree)];
        let mut seen = std::collections::HashSet::new();
        while let Some((id, expected)) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let bytes = self.get_raw_verified(id)?;
            let actual = decode_object_type(&bytes)?;
            if actual != expected {
                return Err(Error::Corrupt(format!(
                    "typed edge expected {}, found {} at {id}",
                    expected.as_str(),
                    actual.as_str()
                )));
            }
            out.push(id);
            match actual {
                ObjectType::Tree => {
                    let t = Tree::decode(&bytes)?;
                    for e in t.entries {
                        let expected = match e.kind {
                            EntryKind::Blob => ObjectType::Blob,
                            EntryKind::Tree => ObjectType::Tree,
                        };
                        stack.push((e.id, expected));
                    }
                }
                ObjectType::Blob => {
                    Blob::decode(&bytes)?;
                }
                other => {
                    return Err(Error::Corrupt(format!(
                        "unexpected {} in tree walk at {id}",
                        other.as_str()
                    )));
                }
            }
        }
        Ok(out)
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
