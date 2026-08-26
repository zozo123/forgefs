#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


# Object identity stays centralized in forge-core: expose the same BLAKE3-256
# computation over a reader rather than making forge-store depend on blake3.
replace_once(
    "crates/forge-core/src/object.rs",
    "use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};\n",
    "use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};\nuse std::io::Read;\n",
)
replace_once(
    "crates/forge-core/src/object.rs",
    '''pub fn hash_parts(parts: &[&[u8]]) -> ObjectId {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p);
    }
    ObjectId(*h.finalize().as_bytes())
}
''',
    '''pub fn hash_parts(parts: &[&[u8]]) -> ObjectId {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p);
    }
    ObjectId(*h.finalize().as_bytes())
}

/// Identity of all bytes read from `reader`, without materializing them.
///
/// This is the streaming twin of [`hash_bytes`]: both are exactly BLAKE3-256,
/// so storage can re-prove I3/I15 with fixed memory while object identity still
/// has one implementation in forge-core.
pub fn hash_reader(reader: &mut impl Read) -> std::io::Result<ObjectId> {
    const BUFFER_BYTES: usize = 64 * 1024;
    let mut h = blake3::Hasher::new();
    let mut buffer = [0u8; BUFFER_BYTES];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        h.update(&buffer[..n]);
    }
    Ok(ObjectId(*h.finalize().as_bytes()))
}
''',
)

replace_once(
    "crates/forge-store/src/blob.rs",
    "use forge_core::{hash_bytes, hash_parts};\n",
    "use forge_core::{hash_parts, hash_reader};\n",
)

replace_once(
    "crates/forge-store/src/blob.rs",
    '''    fn verify_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
        let bytes = fs::read(path).map_err(|e| Error::Io(e.to_string()))?;
        if hash_bytes(&bytes) != id {
            return Err(Error::Corrupt(format!(
                "existing object does not match its id: {id}"
            )));
        }
        Ok(())
    }

    fn verify_and_sync_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
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
        sync_file_counted(
            &file,
            &self.stats,
            crate::DurabilityBarrier::ObjectExistingFile,
        )?;
        Ok(())
    }
''',
    '''    fn verify_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
        let mut file = fs::File::open(path)?;
        verify_object_stream(id, &mut file)
    }

    fn verify_and_sync_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
        // Verify and force the same descriptor. Streaming the rehash keeps I3's
        // trust-boundary proof while removing the object-sized allocation that
        // made an idempotent put cost one extra payload of live memory (#362).
        // Opening writable is intentional: macOS F_FULLFSYNC is a fail-closed
        // durability contract, not a best-effort read hint.
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        verify_object_stream(id, &mut file)?;
        sync_file_counted(
            &file,
            &self.stats,
            crate::DurabilityBarrier::ObjectExistingFile,
        )?;
        Ok(())
    }
''',
)

marker = '''/// A durable object must be a regular file. Without this check a FIFO planted
/// at an object path made `fs::read` block forever, so fsck/export/import hung
/// indefinitely instead of failing closed -- while a mere byte flip in the same
/// file was correctly reported as corruption.
'''
replace_once(
    "crates/forge-store/src/blob.rs",
    marker,
    '''/// Re-prove an existing object's identity without allocating its size in RAM.
/// The caller chooses the descriptor (read-only or the exact descriptor that
/// will subsequently be forced); the bytes are hashed by forge-core so the
/// canonical ObjectId implementation remains single-sourced.
fn verify_object_stream(id: ObjectId, reader: &mut impl Read) -> Result<()> {
    let actual = hash_reader(reader)?;
    if actual != id {
        return Err(Error::Corrupt(format!(
            "existing object does not match its id: {id}"
        )));
    }
    Ok(())
}

'''+marker,
)

store_marker = '''/// The object plane is a type parameter, not a file layout. `Store` is written
/// against [`ObjectStore`] and defaults to the only production implementation,
/// so the bare name `Store` still means `Store<LocalBlobStore>` everywhere it
/// did before -- while the compiler now proves this type uses nothing but the
/// trait.
'''
replace_once(
    "crates/forge-store/src/lib.rs",
    store_marker,
    '''/// Maximum durable-object bytes retained by one Store's hot raw-object cache.
///
/// Entry count is still bounded as a defense against tiny-object metadata
/// overhead, but bytes are the primary memory invariant: one VERSION 1 Blob can
/// be much larger than an ordinary source file. Objects larger than the whole
/// budget are deliberately not cached. This is a performance policy only;
/// trust-boundary reads still bypass the cache (I15).
pub const OBJECT_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const OBJECT_CACHE_MAX_ENTRIES: usize = 256;

struct BlobCache {
    entries: LruCache<ObjectId, Arc<[u8]>>,
    bytes: usize,
    budget_bytes: usize,
    max_entries: usize,
}

impl BlobCache {
    fn new() -> Self {
        Self::with_limits(OBJECT_CACHE_MAX_ENTRIES, OBJECT_CACHE_BUDGET_BYTES)
    }

    fn with_limits(max_entries: usize, budget_bytes: usize) -> Self {
        let capacity = NonZeroUsize::new(max_entries).expect("non-zero object cache capacity");
        Self {
            entries: LruCache::new(capacity),
            bytes: 0,
            budget_bytes,
            max_entries,
        }
    }

    fn get(&mut self, id: &ObjectId) -> Option<&Arc<[u8]>> {
        self.entries.get(id)
    }

    fn put(&mut self, id: ObjectId, value: Arc<[u8]>) {
        if let Some(previous) = self.entries.pop(&id) {
            self.bytes = self.bytes.saturating_sub(previous.len());
        }
        let len = value.len();
        if len > self.budget_bytes {
            return;
        }
        while self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(len) > self.budget_bytes
        {
            let Some((_evicted_id, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.len());
        }
        self.bytes = self.bytes.saturating_add(len);
        debug_assert!(self.entries.put(id, value).is_none());
    }

    fn pop(&mut self, id: &ObjectId) {
        if let Some(previous) = self.entries.pop(id) {
            self.bytes = self.bytes.saturating_sub(previous.len());
        }
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.bytes
    }
}

'''+store_marker,
)
replace_once(
    "crates/forge-store/src/lib.rs",
    "    blob_cache: Mutex<LruCache<ObjectId, Arc<[u8]>>>,\n",
    "    blob_cache: Mutex<BlobCache>,\n",
)
replace_once(
    "crates/forge-store/src/lib.rs",
    "            blob_cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),\n",
    "            blob_cache: Mutex::new(BlobCache::new()),\n",
)

lib = Path("crates/forge-store/src/lib.rs")
text = lib.read_text()
if "mod cache_tests" in text:
    raise SystemExit("cache test module already exists")
lib.write_text(text + '''

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn bytes(n: usize) -> Arc<[u8]> {
        Arc::from(vec![0u8; n].into_boxed_slice())
    }

    #[test]
    fn raw_object_cache_is_bounded_by_bytes_and_entries() {
        let mut cache = BlobCache::with_limits(2, 6);
        let a = ObjectId([1; 32]);
        let b = ObjectId([2; 32]);
        let c = ObjectId([3; 32]);

        cache.put(a, bytes(4));
        cache.put(b, bytes(2));
        assert_eq!(cache.retained_bytes(), 6);
        assert!(cache.get(&a).is_some());

        // `a` was just touched, so `b` is evicted first; the byte ceiling then
        // also evicts `a` before the 3-byte entry can fit.
        cache.put(c, bytes(3));
        assert!(cache.get(&a).is_none());
        assert!(cache.get(&b).is_none());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.retained_bytes(), 3);

        // One object larger than the entire budget is never retained.
        cache.put(a, bytes(7));
        assert!(cache.get(&a).is_none());
        assert_eq!(cache.retained_bytes(), 3);
    }
}
''')

replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    '''    // Phase 2 - republishing identical bytes. Dedup still re-reads the whole
    // durable object to re-prove its hash at the trust boundary (I3), so it
    // costs one payload. Characterised, not endorsed.
    let (again, dedup) = peak_payloads(|| store.put_blob_data(&data).unwrap());
    assert_eq!(again, id);
    assert!(
        (0.75..1.5).contains(&dedup),
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \\
         the characterised cost is one payload for the verifying re-read"
    );
''',
    '''    // Phase 2 - republishing identical bytes. I3 still re-reads and hashes
    // every durable byte, but verification is streaming through a fixed 64 KiB
    // buffer, so an idempotent put no longer allocates one payload of its own.
    let (again, dedup) = peak_payloads(|| store.put_blob_data(&data).unwrap());
    assert_eq!(again, id);
    assert!(
        dedup < 0.25,
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \\
         streaming verification must stay independent of object size"
    );
''',
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    '''    // Phase 3 - reading it back from a cold store. Three payloads are live at
    // once: the durable read buffer, the clone the object cache keeps, and the
    // decoded copy handed to the caller. That cache is bounded by entry count
    // (256), never by bytes, so 256 large objects are 256 payloads resident.
    // This is the half of issue #11 that a chunked read path would have to beat.
''',
    '''    // Phase 3 - reading it back from a cold store. This 8 MiB object fits the
    // 64 MiB raw-object cache budget, so three payloads are still live at once:
    // the durable read buffer, the cache clone, and the decoded copy returned.
    // Objects larger than the byte budget are deliberately not retained; the
    // remaining 3x cold-read peak is item 3 of #362, not an unbounded cache.
''',
)

# Update the measured design note, leaving items 3 and 5 explicitly open.
doc = Path("docs/CHUNKING.md")
text = doc.read_text()
old = "| `put_blob_data`, identical bytes again | 1.00x | 1.00x | `verify_existing` re-reads the whole durable object to re-prove its hash (I3) |"
new = "| `put_blob_data`, identical bytes again | 1.00x | **~0.00x** | full durable rehash remains (I3), now through a fixed 64 KiB buffer |"
if old not in text:
    raise SystemExit("CHUNKING allocator table changed")
text = text.replace(old, new, 1)
old = '''**The object cache is bounded by entry count, not by bytes.**
`Store::blob_cache` is `LruCache<ObjectId, Arc<[u8]>>` with capacity 256
(`crates/forge-store/src/lib.rs:254`). One entry can be a 164 MiB object. A
`checkin` or `fsck` that walks 256 large blobs pins 256 payloads in memory. This
is a memory hazard independent of object size policy, and chunking would *hide*
it rather than fix it (256 chunk-sized entries is a small number). It should be
given a byte budget on its own merits.'''
new = '''**The object cache now has a byte budget as well as an entry budget.**
`Store::blob_cache` retains at most 64 MiB and 256 entries; an individual raw
object larger than 64 MiB is served but not cached. This closes the independent
retention hazard: walking many large blobs can no longer pin 256 payloads in
RAM. The limit is performance policy only and does not change I15 verification.'''
if old not in text:
    raise SystemExit("CHUNKING cache finding changed")
text = text.replace(old, new, 1)
old = '''2. Streaming dedup verify: rehash `verify_existing` /
   `verify_and_sync_existing` in a fixed buffer. Removes the remaining 1.00x on
   republishing identical bytes. No trust change: the same bytes are read and
   the same hash is compared.'''
new = '''2. *(done)* Streaming dedup verify: `verify_existing` /
   `verify_and_sync_existing` rehash in a fixed 64 KiB buffer. The same bytes
   are read and the same ObjectId is compared; only the payload-sized allocation
   is gone.'''
if old not in text:
    raise SystemExit("CHUNKING item 2 changed")
text = text.replace(old, new, 1)
old = "4. Give `Store::blob_cache` a byte budget. Independent of everything above."
new = "4. *(done)* `Store::blob_cache` has a 64 MiB byte budget in addition to its\n   256-entry cap; over-budget individual objects are not retained."
if old not in text:
    raise SystemExit("CHUNKING item 4 changed")
text = text.replace(old, new, 1)
doc.write_text(text)
