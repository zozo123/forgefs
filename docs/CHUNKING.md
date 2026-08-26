# Chunked blobs and Merkle file objects (issue #11)

Status: **design only, not recommended yet.** This document records the measured
object-size ceiling, the format cost of removing it, and the trigger conditions
that would justify paying that cost. Three format-neutral parts of the ceiling
have now been removed: copy-free publication, streaming dedup verification,
streaming typed Blob validation in provenance walks, and a byte-bound raw-object
cache.

`FORMAT.md` freezes the VERSION 1 encoding, and `v0.1.0` is now a real release
tag, so FORMAT.md's pre-release exception has closed. Chunking needs a new
object type and a new tree entry kind, which is a repository VERSION bump. That
is the expensive part of this proposal, and the reason the recommendation is
"not yet".

## 1. The ceiling, measured

Method: `crates/forge-cli` debug binary, Linux, 4 vCPU / 8 GiB. Each operation
runs as its own process; peak RSS is `ru_maxrss` from `wait4`. The multiplier is
`(peak_rss - 10.8 MB baseline) / N`, where N is the size of one blob written at
one path. Numbers are stable to two decimals from N = 64 MiB upward.

Repository state at measurement: `63fa098` (`main`, after #315).

| N | write | checkin | read | export | fsck --full | import |
|---|---|---|---|---|---|---|
| 64 MiB | 3.00x | 3.00x | 3.00x | 3.00x | 2.00x | 3.00x |
| 128 MiB | 3.00x | 3.00x | 3.00x | 3.00x | 2.00x | 3.00x |
| 256 MiB | 3.00x | 3.00x | 3.00x | 3.00x | 2.00x | 3.00x |

Raw peak RSS at N = 256 MiB: write 815,955,968 B; fsck --full 547,758,080 B.

### Where the ceiling actually is

Bisection under `RLIMIT_AS = 512 MiB` (the child is `fork`ed with the limit set,
so an over-budget allocation fails deterministically instead of racing the OOM
killer). "OK" means all six operations above succeed.

```
largest N where every operation succeeds : 164 MiB
smallest N where any operation fails     : 165 MiB   (write)
```

The failure is an allocation abort, and the failing size names the culprit:

```
165 MiB: memory allocation of 173015056 bytes failed    (= N + 16, the encode buffer)
257 MiB: memory allocation of 269484032 bytes failed    (= N exactly, the to_vec)
```

Issue #11 reports `(144, 152] MB` on a 512 MB box. That is the same ceiling
measured against physical RAM rather than address space; the ordering
(RSS ceiling below AS ceiling) is what you would expect once page cache and the
SQLite catalog are competing for the same pages.

`forge write` already warns above 64 MiB
(`crates/forge-api/src/workspace.rs:180`). That threshold was not previously
backed by a measurement. It is roughly 40% of the measured 512-MiB-budget
ceiling, which is a defensible place for a warning; this document is the
missing evidence for it. The raw-object cache now uses the same 64 MiB value as
its total encoded-byte budget, so a single object above the warning threshold
is never duplicated into that cache.

### Allocator-level accounting

`crates/forge-store/tests/large_blob_memory.rs` measures peak *live* bytes with
a counting global allocator (deterministic, and unaffected by the allocator
returning pages to the OS; SQLite allocates through C `malloc` and so does not
pollute the reading). At N = 8 MiB, peak extra live bytes as a multiple of N:

| Operation | Before | After | Where it goes |
|---|---|---|---|
| `put_blob_data`, first publication | 2.00x | **0.00x** | `data.to_vec()` into a temporary `Blob`, then the `encode()` buffer |
| `put_blob_data`, identical bytes again | 1.00x | **<0.25x** | full durable rehash remains (I3), now through a fixed 64 KiB buffer |
| `collect_intros`, one cold 8 MiB Blob | >1.00x | **<0.25x** | full typed/hash validation remains (I1/I15), now through a fixed buffer and tiny canonical frame |
| `get_blob_data`, one cold 8 MiB object | 3.00x | 3.00x | durable read buffer + the cached clone + the decoded copy returned |
| walk ten distinct 8 MiB objects, retained raw cache | >80 MiB | **<64 MiB** | 64 MiB encoded-byte budget plus the pre-existing 256-entry cap |

(The process-level multipliers in the first table are one higher than these,
because they include the caller's own copy of the payload.)

### Two findings that are not about chunking

**The `checkin` typed-walk payload copy is resolved without weakening the
check.** `Store::intro_walk` now asks the object backend to verify Blob edges
without materializing their contents. The local backend re-hashes every durable
byte through the same fixed 64 KiB identity path, then compares the small frame
against `blob_frame_prefix(payload_len)`, proving type, canonical header,
declared size and file length (I1/I15). Trees still decode normally. The
allocator regression walks an 8 MiB Blob from a cold store and holds peak extra
live memory below 0.25x the payload.

**The object-cache memory hazard is resolved without chunking.** The raw object
cache still has the original 256-entry LRU cap, but it now also tracks the
encoded bytes held by those entries and evicts LRU entries above 64 MiB. An
individual object larger than that budget bypasses the cache. The bound is on
object bytes rather than allocator overhead; entry metadata remains separately
bounded by 256. `large_blob_memory.rs` fills the cache past the byte ceiling,
checks retained live memory, checks transient peak memory, and then proves the
oldest object was really evicted by observing a cache miss. Chunking is no
longer needed to prevent 256 large payloads being pinned in one `Store`.

## 2. What already changed

Publishing a blob no longer copies the payload. The VERSION 1 blob file is a
16-byte frame followed by the payload verbatim, so the publisher can hash and
write the caller's buffer in place:

- `forge_core::blob_frame_prefix(size)` exposes the split point. `prefix ++ data`
  is byte-for-byte `Blob { data }.encode()`.
- `forge_core::hash_parts(&[..])` hashes a concatenation without materialising it.
- `PublishBatch::put_parts` / `LocalBlobStore::put_parts` / `Store::put_raw_parts`
  write the parts in order behind the same fsync barriers and the same dedup
  path as `put`.

Nothing about the encoding, the ObjectId, the durability barriers or the CLI
ABI moved. Measured effect on `forge write` at N = 256 MiB:
815,955,968 B -> 279,150,592 B peak RSS, i.e. **3.00x -> 1.00x**.

The raw-object cache is also byte-bound now: `OBJECT_CACHE_MAX_BYTES` is 64 MiB,
in addition to the existing 256-entry cap. This changes only memory residency;
cache misses still reread and re-hash through the same object-store path, and
I15 trust-boundary reads still bypass the cache entirely.

These changes do not make chunking necessary or change VERSION 1. Copy-free
publication moves the publish half of the ceiling from RAM/3 to RAM/1; the cache
bound removes unbounded accumulation across a walk. The remaining single-object read/export copies are the format-neutral work
below.

## 3. If chunking were built

### Representation

A new object type `0x07` "File", empty payload, header
`{chunks: [bstr32...], size: uint}` where each `chunks` item names a Blob and
`size` is the total logical length. Files larger than one chunk list holds spill
to a second level: a File whose `chunks` name Files rather than Blobs, with the
level distinguished by the type of the referenced object at verification time,
not by a header flag (fail closed on a mixed level).

`Tree` needs a new `EntryKind` for "regular file stored as a File object".
Adding a kind to the tree entry grammar means a VERSION 1 reader decoding a
VERSION 2 tree hits an unknown kind and, correctly, calls it Corrupt (I1). That
is the whole compatibility problem in one sentence.

### Chunk size

Content-defined chunking (FastCDC, or Gear/AE) with **fixed, versioned**
parameters. The usual literature default of a 64 KiB average is wrong for
ForgeFS: every object here is published with its own `fsync` on the file plus a
barrier on its shard directory (`crates/forge-store/src/blob.rs`).
`PublishBatch` coalesces the *directory* barriers per shard, but the per-file
barrier stays 1:1 with objects. At a 64 KiB average, one 1 GiB file is ~16,000
objects and ~16,000 file barriers; the measured barrier counters in
`BlobStoreStats` are exactly the thing that would go through the roof.

Proposed: min 256 KiB, average 1 MiB, max 4 MiB, normalization level 2. One
1 GiB file is then ~1,024 objects. This is close to restic's tuning and, unlike
borg's 2 MiB, still finds useful boundaries in mid-size binaries. The threshold
below which a file stays a single Blob should be the max chunk size (4 MiB), not
the average, so that a file is either one Blob or at least two chunks and there
is no ambiguous middle.

### Preserving I1 and I2

I2 says one logical object is one byte string is one ObjectId. Chunking
threatens it directly: the same file content could be a Blob *or* a File, giving
two ObjectIds for the same bytes, which also destroys tree-level dedup and
`put` idempotence (I3).

The only sound answer is that **the representation must be a total function of
the content**, not a choice:

- content of length <= 4 MiB: exactly one Blob. A File object of that length is
  Corrupt.
- content longer than 4 MiB: exactly one File, whose `chunks` are the FastCDC
  boundaries under the frozen parameter set. A Blob of that length reachable
  from a tree is Corrupt in a VERSION 2 repository; a File whose chunk list does
  not reproduce under re-chunking is Corrupt.

That makes identity canonical by construction and gives fsck something real to
check. The price is that the FastCDC parameter set (gear table, min/avg/max,
normalization) becomes frozen format, exactly like the CBOR subset. Retuning it
later is another VERSION bump. That price should be acknowledged out loud in
FORMAT.md rather than discovered later.

### What it does to fsck

- Every chunk is an ordinary Blob, so hash verification is unchanged; the object
  count per `--full` scan grows by (bytes / 1 MiB).
- New checks: `size` equals the sum of chunk lengths; each `chunks` item resolves
  to an object of the right type; the level structure is not mixed.
- The canonical-boundary check (re-run FastCDC and compare) is a second full
  pass over the bytes on top of the BLAKE3 pass. It should be behind its own
  flag, not inside `--full`, or `fsck --full` doubles in wall time for chunked
  repositories.
- Today `fsck --full` peaks at 2.00x the largest object. With chunking it peaks
  at 2.00x the largest *chunk*, i.e. 8 MiB. That is the one place chunking
  straightforwardly wins.

### What it does to the seal provenance walk

This is the part most likely to be underestimated.

`FORMAT.md` fixes `Snapshot.prov` as a manifest whose key set is *every Tree and
Blob reachable from `Snapshot.tree`*, plus every reachable Contribution, each
with an attribution string; the caps are 1,000,000 entries and a 64 MiB
canonical payload, and I15 makes a missing or extra key corruption.

Chunking multiplies the Tree/Blob key set by (file size / 1 MiB). At the entry
cap, a repository saturates the manifest at roughly 1 TiB of chunked content
even before the 64 MiB payload cap bites. Every seal would also pay an
attribution lookup per chunk.

So chunking forces a decision about what provenance means:

- **Option A (attribute the File, not its chunks).** The manifest key set
  becomes Trees, Files, and Blobs that are not *only* reachable as chunks. This
  keeps manifests the size they are today and matches intuition ("this file came
  from agent X"), but it is a semantic narrowing of I15's "exact provenance
  scope" and must be written into FORMAT.md as manifest version 2, with the
  version 1 shape still readable. The signature still binds the whole content,
  because a File's ObjectId covers its chunk list and each chunk id covers its
  bytes; what is lost is a per-chunk *attribution*, which nobody has asked for.
- **Option B (attribute every chunk).** Honest but expensive, and it makes the
  manifest caps a hard object-size limit in disguise.

Option A is the right answer, and the fact that it needs a manifest version bump
on top of the repository VERSION bump is a good illustration of how far this
change reaches. `Store::collect_intros` / `intro_walk`, which populates the
first-introducer hints those attributions come from, has to grow the same
Option-A shortcut or checkin cost becomes linear in chunk count.

### Migration

`I17` and `FORMAT.md` both forbid rewriting existing object files or reinterpreting
existing ObjectIds. So there is no in-place conversion of existing large blobs.
The workable story is:

1. A VERSION 2 binary reads VERSION 1 repositories unchanged, including
   oversized single Blobs. Existing OIDs and the checked-in canonical fixtures
   are untouched.
2. `.forge/VERSION` moves to `2` only for repositories created by a VERSION 2
   binary, or by an explicit, logged, opt-in stamp. A VERSION 1 binary then
   fails closed on that repository, which is the correct and expected outcome.
3. The "content over 4 MiB must be a File" rule is enforced on *write*. fsck
   cannot enforce it retroactively, because a legacy oversized Blob is
   indistinguishable from a rule violation by bytes alone. It must therefore be
   a distinct, non-corrupt fsck finding class ("legacy unchunked object"), not
   `Corrupt`.

Point 3 is the weak seam in the I2-by-construction argument, and it should be
stated in the proposal rather than glossed: in a repository that has ever been
VERSION 1, the "one content, one representation" property holds for new writes
only.

## 4. The counter-argument, and the recommendation

**Source code files are small.** The workload ForgeFS exists for -- many
concurrent agents editing a source tree -- has a median file in the low
kilobytes. At those sizes chunking is pure overhead: an extra object type, an
extra indirection per read, a larger provenance manifest, and a frozen CDC
parameter set, in exchange for nothing. Every measurement above is about a file
class that the core use case does not contain.

**Where the ceiling really binds**: committed model weights, build artifacts,
container layers, datasets, captured traces. For those, the prior question is
whether they belong in ForgeFS at all. `AGENTS.md` states the product boundary
plainly -- ForgeFS is "not a general POSIX filesystem ... or an eventually
consistent object store". Chunked Merkle files are precisely the feature that
turns a truth-and-convergence layer into a general blob store. That is a product
decision, not a performance one, and it should be made deliberately.

**Recommendation: do not build chunking now.** Do the format-neutral work
instead, in this order:

1. *(done)* copy-free publish. Measured 3.00x -> 1.00x on `forge write`.
2. *(done)* Streaming dedup verify: `verify_existing` /
   `verify_and_sync_existing` rehash through a fixed 64 KiB buffer. The same
   durable bytes are read and the same ObjectId is compared; the sync path uses
   the same descriptor it hashes, so the I4 proof is unchanged.
3. Streaming read and export: a `Store` entry point that copies object bytes to
   a sink in fixed-size reads while hashing. This is the 3.00x -> ~0x change for
   `read`, `export` and `import`, and it is where most of the remaining ceiling
   is. **Caveat:** streaming to a sink before the hash is verified is a trust
   regression under I15 unless the sink is staged and published only after the
   hash matches. `export_tar` already writes a sibling and publishes atomically,
   so it can take this safely; `forge read` writing to a pipe cannot, and should
   keep buffering or grow an explicit `--unverified-stream` opt-out. Do not
   quietly weaken I15 to win a benchmark.
4. *(done)* Byte-bound `Store::blob_cache`: 64 MiB of encoded object bytes plus
   the existing 256-entry cap; larger single objects bypass the cache.
5. *(done)* Stop `intro_walk` pulling whole blob payloads through the
   object cache. Blob edges retain full typed and hash validation through the
   `ObjectStore::verify_blob` seam, but the local implementation uses fixed
   memory and never inserts the payload into the raw-object cache.

Together those take the ceiling from roughly RAM/3 to roughly RAM, i.e. a 3x,
with zero format risk. A 3x that costs nothing beats a 10x that costs a
repository VERSION.

### Trigger conditions

Revisit chunking when any of these is true, and not before:

1. **Size.** A real workload needs a single object larger than about half the
   memory of the smallest supported agent container, *after* items 1-3 above are
   done. Until then the answer is "raise the ceiling, do not change the format".
2. **Duplication.** Measured: the fraction of object bytes attributable to files
   over 16 MiB that have a same-path predecessor differing in less than ~20% of
   their bytes. Below roughly 20% of total stored bytes, chunk-level dedup and
   delta transfer buy less than the complexity costs. This is measurable today
   with `forge fsck --full --json` plus object sizes; nobody has measured it.
3. **A second reason to bump VERSION.** The repository VERSION bump is the
   dominant cost of this change and should be spent once. If another accepted
   change already requires VERSION 2, chunking should be re-evaluated as part of
   that change rather than on its own.
4. **Transfer, not storage.** If `forge push` / remote sync becomes a real
   workload, chunk-level transfer is worth more than chunk-level storage, and
   the design should be driven from the wire protocol rather than from the
   object format.

## 5. Evidence

| Claim | Where |
|---|---|
| publish allocates no copy of the payload; identity unchanged | `crates/forge-store/tests/large_blob_memory.rs` |
| republish verification and typed intro walks stay below 0.25x one 8 MiB payload | same |
| one cold 8 MiB read costs three payloads | same |
| raw-object cache retains <64 MiB after walking ten distinct 8 MiB blobs | same |
| process-level RSS multipliers and the 512 MiB bisection | this document, section 1 |