#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:160]!r}")
    p.write_text(text.replace(old, new, 1))


# The staged reader hashes the exact bytes tar consumes. BLAKE3 is already a
# workspace dependency and the object identity primitive; this adds no new
# package to the locked graph.
replace_once(
    "crates/forge-store/Cargo.toml",
    "[dependencies]\nforge-core.workspace = true\n",
    "[dependencies]\nblake3.workspace = true\nforge-core.workspace = true\n",
)

# A staged reader may expose bytes before final authentication only because its
# caller promises not to publish the sink until finish() succeeds. The type and
# method names make that trust contract explicit, and #[must_use] makes dropping
# the final verifier noisy.
replace_once(
    "crates/forge-store/src/blob.rs",
    '''pub struct PublishBatch<'a> {
''',
    '''#[must_use = "staged blob bytes are not authenticated until finish() succeeds"]
pub struct StagedBlobReader {
    file: fs::File,
    id: ObjectId,
    hasher: blake3::Hasher,
    remaining: u64,
    payload_len: u64,
    object_len: u64,
    stats: Arc<BlobStoreCounters>,
}

impl StagedBlobReader {
    pub fn payload_len(&self) -> u64 {
        self.payload_len
    }

    /// Complete the I15 proof for bytes already copied to a staged sink.
    ///
    /// A caller must publish that sink only after this returns `Ok(())`. A
    /// mismatch is deliberately detected after the final payload byte so one
    /// pass over a large Blob is enough.
    pub fn finish(mut self) -> Result<()> {
        if self.remaining != 0 {
            return Err(Error::Corrupt(format!(
                "blob {} ended with {} payload bytes unread",
                self.id, self.remaining
            )));
        }
        let mut extra = [0u8; 1];
        if self.file.read(&mut extra)? != 0 {
            return Err(Error::Corrupt(format!(
                "blob {} grew while staged output was being built",
                self.id
            )));
        }
        let actual = ObjectId(*self.hasher.finalize().as_bytes());
        if actual != self.id {
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {}", self.id)));
        }
        self.stats
            .get_bytes
            .fetch_add(self.object_len, Ordering::Relaxed);
        Ok(())
    }
}

impl Read for StagedBlobReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let limit = usize::try_from(self.remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = self.file.read(&mut buf[..limit])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("blob {} payload truncated", self.id),
            ));
        }
        self.hasher.update(&buf[..n]);
        self.remaining -= n as u64;
        Ok(n)
    }
}

pub struct PublishBatch<'a> {
''',
)

# Factor the tiny canonical frame proof so ordinary typed verification can hash
# first, while staged export can validate the frame/size before emitting bytes
# and finish the content hash as the payload is consumed.
old_verify = '''    /// Verify one Blob's durable identity and canonical v1 frame without
    /// materializing its payload. This is the typed-graph trust check used by
    /// `Store::intro_walk` (I1/I15).
    pub fn verify_blob(&self, id: ObjectId) -> Result<()> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file =
            fs::File::open(&path).map_err(|_| Error::NotFound(format!("object {id}")))?;

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
'''
new_verify = '''    /// Verify one Blob's durable identity and canonical v1 frame without
    /// materializing its payload. This is the typed-graph trust check used by
    /// `Store::intro_walk` (I1/I15).
    pub fn verify_blob(&self, id: ObjectId) -> Result<()> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file =
            fs::File::open(&path).map_err(|_| Error::NotFound(format!("object {id}")))?;

        let actual = hash_reader(&mut file)?;
        let len = file.stream_position()?;
        if actual != id {
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        self.stats.get_bytes.fetch_add(len, Ordering::Relaxed);
        verify_blob_frame(&mut file, len)?;
        Ok(())
    }

    /// Open a Blob payload for a sink that is itself staged and unpublished.
    ///
    /// The canonical frame and payload length are checked up front, but payload
    /// authentication completes only in [`StagedBlobReader::finish`]. This is
    /// intentionally *not* the API for stdout, sockets, or any irreversible
    /// sink: callers must discard their staged output when `finish` fails.
    pub fn open_blob_payload_for_staged_output(&self, id: ObjectId) -> Result<StagedBlobReader> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file =
            fs::File::open(&path).map_err(|_| Error::NotFound(format!("object {id}")))?;
        let object_len = file.metadata()?.len();
        let (prefix, payload_len) = verify_blob_frame(&mut file, object_len)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&prefix);
        Ok(StagedBlobReader {
            file,
            id,
            hasher,
            remaining: payload_len,
            payload_len,
            object_len,
            stats: Arc::clone(&self.stats),
        })
    }
'''
replace_once("crates/forge-store/src/blob.rs", old_verify, new_verify)

# Insert the canonical frame verifier beside the streaming identity verifier.
marker = '''/// Re-prove an existing object's identity with memory independent of its size.
/// The caller owns descriptor choice: the durability path passes the exact
/// descriptor it will subsequently force, so verified bytes and synced bytes
/// cannot diverge through a pathname reopen.
fn verify_object_stream(id: ObjectId, reader: &mut impl Read) -> Result<()> {
'''
helper = '''/// Validate the complete v1 Blob frame without reading or allocating the
/// payload. On success the descriptor is positioned at the first payload byte.
fn verify_blob_frame(file: &mut fs::File, object_len: u64) -> Result<(Vec<u8>, u64)> {
    if object_len < 5 {
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
    if header_end > object_len {
        return Err(Error::Corrupt("object header truncated".into()));
    }
    let payload_len = object_len - header_end;
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
    Ok((expected, payload_len))
}

'''+marker
replace_once("crates/forge-store/src/blob.rs", marker, helper)

# Re-export the staged reader and expose it only on the concrete local Store.
replace_once(
    "crates/forge-store/src/lib.rs",
    "pub use blob::{BlobStoreStats, DirectoryBarrier, GcObjectGuard, LocalBlobStore, PublishBatch};\n",
    "pub use blob::{\n    BlobStoreStats, DirectoryBarrier, GcObjectGuard, LocalBlobStore, PublishBatch, StagedBlobReader,\n};\n",
)
replace_once(
    "crates/forge-store/src/lib.rs",
    '''    pub fn open_read_only(root: &Path) -> Result<Self> {
        let blobs = LocalBlobStore::open_read_only(root.to_path_buf())?;
        let meta = Meta::open_read_only(&root.join("meta.sqlite"))?;
        Ok(Self::with_object_store(root.to_path_buf(), blobs, meta))
    }
''',
    '''    pub fn open_read_only(root: &Path) -> Result<Self> {
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
''',
)

# Executable memory proof: the staged one-pass payload stream must not allocate
# proportional to the Blob, while ordinary trusted get remains buffered by
# design for irreversible callers.
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "fn large_blob_cost_cache_residency_and_intro_walk_are_bounded() {\n",
    "fn large_blob_cost_cache_intro_walk_and_staged_stream_are_bounded() {\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    '''    drop(intro_cold);

    // Phase 4 - reading one object from a cold store. At 8 MiB the object is
''',
    '''    drop(intro_cold);

    // Phase 4 - staged output may stream before authentication completes only
    // because the caller promises to publish its sink after finish(). The
    // reader hashes exactly what it yields and needs no payload-sized buffer.
    let stream_cold = Store::open(a.path()).unwrap();
    let ((), stream_peak) = peak_payloads(|| {
        let mut reader = stream_cold
            .open_blob_payload_for_staged_output(id)
            .unwrap();
        assert_eq!(reader.payload_len(), N as u64);
        std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
        reader.finish().unwrap();
    });
    assert!(
        stream_peak < 0.25,
        "staged stream of a {N}-byte blob peaked at {stream_peak:.2}x the payload; \\
         output streaming must stay independent of object size"
    );
    drop(stream_cold);

    // Phase 5 - reading one object from a cold store. At 8 MiB the object is
''',
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "    // Phase 5 below is the important bound: walking many such blobs no longer\n",
    "    // Phase 6 below is the important bound: walking many such blobs no longer\n",
)
replace_once(
    "crates/forge-store/tests/large_blob_memory.rs",
    "    // Phase 5 - the raw-object LRU is bounded by bytes as well as entries.\n",
    "    // Phase 6 - the raw-object LRU is bounded by bytes as well as entries.\n",
)

# Tar output is already a sibling temporary artifact. Stream into that temporary,
# then authenticate the exact payload bytes before the outer function renames it.
replace_once(
    "crates/forge-api/src/export.rs",
    '''            EntryKind::Blob => {
                let data = store.get_blob_data(e.id)?;
                let mut h = Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if e.exec { 0o755 } else { 0o644 });
                h.set_mtime(0);
                h.set_uid(0);
                h.set_gid(0);
                h.set_username("").ok();
                h.set_groupname("").ok();
                h.set_size(data.len() as u64);
                b.append_data(&mut h, &path, data.as_slice())
                    .map_err(|e| Error::Io(e.to_string()))?;
            }
''',
    '''            EntryKind::Blob => {
                // The archive itself is still unpublished at this point. Read
                // the payload once through a hashing reader, append those exact
                // bytes to the sibling temporary tar, then complete I15 before
                // the outer function can rename the archive into place.
                let mut data = store.open_blob_payload_for_staged_output(e.id)?;
                let mut h = Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if e.exec { 0o755 } else { 0o644 });
                h.set_mtime(0);
                h.set_uid(0);
                h.set_gid(0);
                h.set_username("").ok();
                h.set_groupname("").ok();
                h.set_size(data.payload_len());
                b.append_data(&mut h, &path, &mut data)
                    .map_err(|e| Error::Io(e.to_string()))?;
                data.finish()?;
            }
''',
)

# A late payload corruption is detected only after tar has consumed the staged
# bytes. The final destination and sibling partial must still never survive.
Path("crates/forge-api/tests/export_streaming_trust.rs").write_text(r'''//! I15: staged export may consume a Blob before its final hash comparison, but
//! corrupt bytes must never become a published archive.

use forge_api::Forge;
use forge_types::ObjectId;
use std::fs;
use tempfile::tempdir;

#[test]
fn i15_late_blob_corruption_never_publishes_a_streamed_export() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "work").unwrap();
    let ns = f.session_open(&root, "work").unwrap();
    f.mount(&root, &ns, "/", "ref:work", true).unwrap();
    f.write(&root, &ns, "/large.bin", &vec![0x5a; 256 * 1024], false)
        .unwrap();
    f.checkin(&root, &ns, "/", "seed").unwrap();

    let oid_hex = f
        .ls(&root, &ns, "/")
        .unwrap()
        .into_iter()
        .find(|(name, kind, _, _)| name == "large.bin" && kind == "blob")
        .expect("checked-in blob must be listed")
        .2;
    let oid = ObjectId::from_hex(&oid_hex).unwrap();
    let (a, b) = oid.shard_dirs();
    let object = f.root().join("objects").join(a).join(b).join(oid.hex());

    // Flip the last payload byte, not the frame. The staged reader therefore
    // writes the full tar member and discovers corruption only in finish().
    let mut bytes = fs::read(&object).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&object, bytes).unwrap();

    let out = d.path().join("corrupt.tar");
    let err = f.export_tar(&root, "work", &out).unwrap_err().to_string();
    assert!(err.contains("hash mismatch"), "unexpected export error: {err}");
    assert!(!out.exists(), "corrupt staged bytes reached the final archive");
    let prefix = out.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .count(),
        0,
        "failed streamed export left a sibling partial artifact"
    );
}
''')

# Design ledger: item 3 is resolved for the only safe one-pass sink today.
# Trusted CLI read deliberately remains buffered; that is a security decision,
# not unfinished performance work.
replace_once(
    "docs/CHUNKING.md",
    '''have now been removed: copy-free publication, streaming dedup verification,
streaming typed Blob validation in provenance walks, and a byte-bound raw-object
cache.
''',
    '''have now been removed: copy-free publication, streaming dedup verification,
streaming typed Blob validation in provenance walks, staged one-pass export,
and a byte-bound raw-object cache.
''',
)
replace_once(
    "docs/CHUNKING.md",
    "| `get_blob_data`, one cold 8 MiB object | 3.00x | 3.00x | durable read buffer + the cached clone + the decoded copy returned |\n",
    "| staged payload stream, one cold 8 MiB Blob | n/a | **<0.25x** | exact payload bytes are hashed as copied; staged sink publishes only after `finish()` |\n| `get_blob_data`, one cold 8 MiB object | 3.00x | 3.00x | durable read buffer + the cached clone + the decoded copy returned |\n",
)
replace_once(
    "docs/CHUNKING.md",
    '''The remaining single-object read/export copies are the format-neutral work
below.
''',
    '''The remaining 3.00x `get_blob_data` cost is intentional for trusted reads
that target irreversible sinks: ForgeFS cannot emit bytes before I15 is proved.
Export avoids that constraint because the whole tar is staged and published by
rename only after every streamed Blob finishes verification.
''',
)
replace_once(
    "docs/CHUNKING.md",
    '''3. Streaming read and export: a `Store` entry point that copies object bytes to
   a sink in fixed-size reads while hashing. This is the 3.00x -> ~0x change for
   `read`, `export` and `import`, and it is where most of the remaining ceiling
   is. **Caveat:** streaming to a sink before the hash is verified is a trust
   regression under I15 unless the sink is staged and published only after the
   hash matches. `export_tar` already writes a sibling and publishes atomically,
   so it can take this safely; `forge read` writing to a pipe cannot, and should
   keep buffering or grow an explicit `--unverified-stream` opt-out. Do not
   quietly weaken I15 to win a benchmark.
''',
    '''3. *(done for safe sinks)* Staged streaming export: `export_tar` opens each
   Blob through `StagedBlobReader`, validates its canonical v1 frame, hashes the
   exact payload bytes as tar consumes them, and calls `finish()` before the
   sibling temporary archive can be renamed into place. The allocator gate
   holds an 8 MiB staged stream below 0.25x payload memory, and a late-corruption
   regression proves neither final nor `.partial-*` artifact survives a hash
   mismatch. `forge read` remains buffered on purpose: stdout/a pipe is
   irreversible, so one-pass streaming there would weaken I15. There is no
   implicit unsafe mode.
''',
)
replace_once(
    "docs/CHUNKING.md",
    '''Together those take the ceiling from roughly RAM/3 to roughly RAM, i.e. a 3x,
with zero format risk. A 3x that costs nothing beats a 10x that costs a
repository VERSION.
''',
    '''Together the completed format-neutral slices remove proportional allocation
from publication, dedup verification, checkin provenance walks, and staged tar
export, while bounding cache residency. Trusted direct reads remain buffered by
design because their sink cannot be rolled back. No FORMAT/ObjectId change was
required.
''',
)
replace_once(
    "docs/CHUNKING.md",
    "| one cold 8 MiB read costs three payloads | same |\n",
    "| staged one-pass 8 MiB payload stream stays below 0.25x; trusted buffered read remains 3x by design | same |\n",
)
