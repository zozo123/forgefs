#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1))


# Keep ObjectId computation single-sourced in forge-core, but make the same
# BLAKE3-256 identity available over any Read without materialising the object.
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
/// This is exactly the same BLAKE3-256 identity as [`hash_bytes`] and
/// [`hash_parts`]. Storage uses it to re-prove durable object identity with a
/// fixed memory footprint while keeping ObjectId semantics single-sourced.
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
    "crates/forge-core/src/lib.rs",
    "    blob_frame_prefix, decode_object_type, hash_bytes, hash_parts, parse_file, Blob, Commit,\n",
    "    blob_frame_prefix, decode_object_type, hash_bytes, hash_parts, hash_reader, parse_file, Blob, Commit,\n",
)
replace_once(
    "crates/forge-core/src/lib.rs",
    '''    #[test]
    fn commit_roundtrip() {
''',
    '''    #[test]
    fn streaming_hash_matches_buffered_hash() {
        let data = b"forge streaming identity".repeat(8193);
        let expected = hash_bytes(&data);
        let mut reader = std::io::Cursor::new(&data);
        assert_eq!(hash_reader(&mut reader).unwrap(), expected);

        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(hash_reader(&mut empty).unwrap(), hash_bytes(&[]));
    }

    #[test]
    fn commit_roundtrip() {
''',
)

# Dedup verification continues to re-read every durable byte, but no longer
# creates an object-sized Vec. The sync path hashes the same descriptor it then
# forces, preserving the I4 proof boundary.
replace_once(
    "crates/forge-store/src/blob.rs",
    "use forge_core::{hash_bytes, hash_parts};\n",
    "use forge_core::{hash_bytes, hash_parts, hash_reader};\n",
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
        // Verify and force the same descriptor. Streaming removes the
        // object-sized allocation without weakening the I3/I4 proof boundary.
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
    '''/// Re-prove an existing object's identity with memory independent of its size.
/// The caller owns descriptor choice: the durability path passes the exact
/// descriptor it will subsequently force, so verified bytes and synced bytes
/// cannot diverge through a pathname reopen.
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

# Convert the existing allocator characterization into a regression gate for
# the new bound. The caller-owned 8 MiB payload stays live; the verifier itself
# must add much less than one payload.
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
    // every durable byte, but verification uses a fixed 64 KiB buffer. The
    // verifier therefore must not allocate memory proportional to object size.
    let (again, dedup) = peak_payloads(|| store.put_blob_data(&data).unwrap());
    assert_eq!(again, id);
    assert!(
        dedup < 0.25,
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \\
         streaming verification must stay independent of object size"
    );
''',
)

# Keep the design note as executable bookkeeping: item 2 is done only if the
# allocator gate and full suite above pass on the transformed tree.
replace_once(
    "docs/CHUNKING.md",
    "Two format-neutral parts of the ceiling\nhave now been removed: copy-free publication and a byte-bound raw-object cache.\n",
    "Three format-neutral parts of the ceiling\nhave now been removed: copy-free publication, streaming dedup verification,\nand a byte-bound raw-object cache.\n",
)
replace_once(
    "docs/CHUNKING.md",
    "| `put_blob_data`, identical bytes again | 1.00x | 1.00x | `verify_existing` re-reads the whole durable object to re-prove its hash (I3) |\n",
    "| `put_blob_data`, identical bytes again | 1.00x | **<0.25x** | full durable rehash remains (I3), now through a fixed 64 KiB buffer |\n",
)
replace_once(
    "docs/CHUNKING.md",
    "The remaining single-object\nread and verification copies are the format-neutral work below.\n",
    "The remaining single-object read and typed-walk copies are the\nformat-neutral work below.\n",
)
replace_once(
    "docs/CHUNKING.md",
    '''2. Streaming dedup verify: rehash `verify_existing` /
   `verify_and_sync_existing` in a fixed buffer. Removes the remaining 1.00x on
   republishing identical bytes. No trust change: the same bytes are read and
   the same hash is compared.
''',
    '''2. *(done)* Streaming dedup verify: `verify_existing` /
   `verify_and_sync_existing` rehash through a fixed 64 KiB buffer. The same
   durable bytes are read and the same ObjectId is compared; the sync path uses
   the same descriptor it hashes, so the I4 proof is unchanged.
''',
)
