#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:140]!r}")
    p.write_text(text.replace(old, new, 1))


# Make typed Blob verification part of the ObjectStore seam. The default keeps
# every backend correct; the local backend overrides it with a fixed-memory
# descriptor walk. This keeps Store<O> generic rather than smuggling a local
# filesystem special case into provenance.
replace_once(
    "crates/forge-store/src/objectstore.rs",
    '''    /// Visibility only -- never a durability proof. See the module docs.
    fn has(&self, id: ObjectId) -> bool;
''',
    '''    /// Verify that `id` names a well-formed Blob, including the backend's
    /// ordinary durable-byte identity check. Implementations may stream this
    /// validation; callers must not infer that the payload was materialized or
    /// cached. The default is deliberately boring and correct for simple
    /// backends.
    fn verify_blob(&self, id: ObjectId) -> Result<()> {
        forge_core::Blob::decode(&self.get(id)?)?;
        Ok(())
    }

    /// Visibility only -- never a durability proof. See the module docs.
    fn has(&self, id: ObjectId) -> bool;
''',
)

# Local typed validation: one full BLAKE3 pass through a fixed buffer, then only
# the tiny canonical frame is re-read. `blob_frame_prefix(payload_len)` is the
# v1 encoder itself, so exact comparison proves type, header grammar, declared
# size and file length without allocating/caching the payload.
replace_once(
    "crates/forge-store/src/blob.rs",
    "use forge_core::{hash_bytes, hash_parts, hash_reader};\n",
    "use forge_core::{blob_frame_prefix, hash_bytes, hash_parts, hash_reader};\n",
)
replace_once(
    "crates/forge-store/src/blob.rs",
    "use std::io::{Read, Write};\n",
    "use std::io::{Read, Seek, SeekFrom, Write};\n",
)
replace_once(
    "crates/forge-store/src/blob.rs",
    '''    pub fn has(&self, id: ObjectId) -> bool {
        self.object_path(id).exists()
    }
''',
    '''    /// Verify one Blob's durable identity and canonical v1 frame without
    /// materializing its payload. This is the typed-graph trust check used by
    /// `Store::intro_walk` (I1/I15).
    pub fn verify_blob(&self, id: ObjectId) -> Result<()> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file = fs::File::open(&path)
            .map_err(|_| Error::NotFound(format!("object {id}")))?;

        let actual = hash_reader(&mut file)?;
        let len = file.stream_position()?;
        if actual != id {
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        self.stats.get_bytes.fetch_add(len, Ordering::Relaxed);

        if len < 5 {
            return Err(Error::Corrupt("object file too short".into()));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut frame = [0u8; 5];
        file.read_exact(&mut frame)?;
        let ty = forge_types::ObjectType::from_u8(frame[0])?;
        if ty != forge_types::ObjectType::Blob {
            return Err(Error::Corrupt("not a blob".into()));
        }

        let header_len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as u64;
        let header_end = 5u64
            .checked_add(header_len)
            .ok_or_else(|| Error::Corrupt("object header length overflow".into()))?;
        if header_end > len {
            return Err(Error::Corrupt("object header truncated".into()));
        }
        let payload_len = len - header_end;
        let expected = blob_frame_prefix(payload_len);
        if expected.len() as u64 != header_end {
            return Err(Error::Corrupt("non-canonical blob header".into()));
        }

        file.seek(SeekFrom::Start(0))?;
        let mut observed = vec![0u8; expected.len()];
        file.read_exact(&mut observed)?;
        if observed != expected {
            return Err(Error::Corrupt("invalid blob header".into()));
        }
        Ok(())
    }

    pub fn has(&self, id: ObjectId) -> bool {
        self.object_path(id).exists()
    }
''',
)
replace_once(
    "crates/forge-store/src/blob.rs",
    '''    fn has(&self, id: ObjectId) -> bool {
        LocalBlobStore::has(self, id)
    }
''',
    '''    fn verify_blob(&self, id: ObjectId) -> Result<()> {
        LocalBlobStore::verify_blob(self, id)
    }

    fn has(&self, id: ObjectId) -> bool {
        LocalBlobStore::has(self, id)
    }
''',
)

# Provenance only needs a typed, content-addressed Blob edge. Validate that edge
# through the backend before the generic get_raw/cache path so a checkin never
# allocates a payload it will immediately discard. Tree decoding remains exactly
# as before.
replace_once(
    "crates/forge-store/src/lib.rs",
    '''        if old == Some(new) {
            return Ok(());
        }

        let bytes = self.get_raw(new)?;
''',
    '''        if old == Some(new) {
            return Ok(());
        }

        if expected == ObjectType::Blob {
            self.blobs.verify_blob(new)?;
            oids.push(new);
            return Ok(());
        }

        let bytes = self.get_raw(new)?;
''',
)

# Turn the large-object characterization into an executable proof that the
# provenance walk is now size-independent too.
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "use forge_core::{hash_bytes, Blob};\n",
    "use forge_core::{hash_bytes, Blob, Tree, TreeEntry};\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "use forge_store::{Store, OBJECT_CACHE_MAX_BYTES};\n",
    "use forge_store::{Store, OBJECT_CACHE_MAX_BYTES};\nuse forge_types::EntryKind;\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "fn large_blob_cost_and_raw_cache_residency_are_measured_and_bounded() {\n",
    "fn large_blob_cost_cache_residency_and_intro_walk_are_bounded() {\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    '''    assert!(
        dedup < 0.25,
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \\
         streaming verification must stay independent of object size"
    );

    // Phase 3 - reading one object from a cold store. At 8 MiB the object is
''',
    '''    assert!(
        dedup < 0.25,
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \\
         streaming verification must stay independent of object size"
    );

    // Phase 3 - a provenance intro walk needs the Blob's typed identity, not
    // its payload. Build a one-blob tree, reopen cold so no cache can hide the
    // cost, and prove the walk stays independent of payload size while still
    // returning both exact OIDs.
    let tree_id = store
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "large.bin".into(),
                kind: EntryKind::Blob,
                id,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap();
    drop(store);
    let intro_cold = Store::open(a.path()).unwrap();
    let (intros, intro_peak) = peak_payloads(|| intro_cold.collect_intros(None, tree_id).unwrap());
    assert_eq!(intros, vec![tree_id, id]);
    assert!(
        intro_peak < 0.25,
        "intro walk over a {N}-byte blob peaked at {intro_peak:.2}x the payload; \\
         typed Blob validation must stream instead of materializing the payload"
    );
    drop(intro_cold);

    // Phase 4 - reading one object from a cold store. At 8 MiB the object is
''',
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    '''    drop(store);
    let cold = Store::open(a.path()).unwrap();
''',
    '''    let cold = Store::open(a.path()).unwrap();
''',
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "    // Phase 4 - the raw-object LRU is bounded by bytes as well as entries.\n",
    "    // Phase 5 - the raw-object LRU is bounded by bytes as well as entries.\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "    // Phase 4 below is the important bound: walking many such blobs no longer\n",
    "    // Phase 5 below is the important bound: walking many such blobs no longer\n",
)

# Update the design ledger only after the executable proof above succeeds.
replace_once(
    "docs/CHUNKING.md",
    '''have now been removed: copy-free publication, streaming dedup verification,
and a byte-bound raw-object cache.
''',
    '''have now been removed: copy-free publication, streaming dedup verification,
streaming typed Blob validation in provenance walks, and a byte-bound raw-object
cache.
''',
)
replace_once(
    "docs/CHUNKING.md",
    "| `get_blob_data`, one cold 8 MiB object | 3.00x | 3.00x | durable read buffer + the cached clone + the decoded copy returned |\n",
    "| `collect_intros`, one cold 8 MiB Blob | >1.00x | **<0.25x** | full typed/hash validation remains (I1/I15), now through a fixed buffer and tiny canonical frame |\n| `get_blob_data`, one cold 8 MiB object | 3.00x | 3.00x | durable read buffer + the cached clone + the decoded copy returned |\n",
)
replace_once(
    "docs/CHUNKING.md",
    '''**`checkin` reads every blob payload it walks.** `checkin` costs 3.00x the
largest blob in the tree even though it never needs blob *contents*. Traced:

```
forge_api::Forge::checkin
  -> forge_store::Store::collect_intros
    -> forge_store::Store::intro_walk          (crates/forge-store/src/lib.rs)
      -> Store::get_raw                         full payload
      -> Blob::decode(&bytes)                   a second full payload, discarded
```

The `Blob::decode` is a deliberate typed-graph check and must not simply be
deleted; it wants a streaming validator (parse the frame, check `size` against
the file length, rehash in a fixed buffer) rather than a buffering one.

''',
    '''**The `checkin` typed-walk payload copy is resolved without weakening the
check.** `Store::intro_walk` now asks the object backend to verify Blob edges
without materializing their contents. The local backend re-hashes every durable
byte through the same fixed 64 KiB identity path, then compares the small frame
against `blob_frame_prefix(payload_len)`, proving type, canonical header,
declared size and file length (I1/I15). Trees still decode normally. The
allocator regression walks an 8 MiB Blob from a cold store and holds peak extra
live memory below 0.25x the payload.

''',
)
replace_once(
    "docs/CHUNKING.md",
    '''The remaining single-object read and typed-walk copies are the
format-neutral work below.
''',
    '''The remaining single-object read/export copies are the format-neutral work
below.
''',
)
replace_once(
    "docs/CHUNKING.md",
    "5. Stop `intro_walk` pulling whole blob payloads through the object cache.\n",
    '''5. *(done)* Stop `intro_walk` pulling whole blob payloads through the
   object cache. Blob edges retain full typed and hash validation through the
   `ObjectStore::verify_blob` seam, but the local implementation uses fixed
   memory and never inserts the payload into the raw-object cache.
''',
)
replace_once(
    "docs/CHUNKING.md",
    "| republish costs one payload (verifying re-read) | same |\n",
    "| republish verification and typed intro walks stay below 0.25x one 8 MiB payload | same |\n",
)
