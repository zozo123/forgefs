# The ObjectStore seam

ForgeFS already made the object-storage bet: immutable, content-addressed,
write-once objects, and one tiny mutable ref published by compare-and-swap.
That is the commit protocol object stores converged on. What made ForgeFS read
as local-only was not the bet, it was the vocabulary -- the object plane was
spelled in files, `fsync`, hard links and shard directories, with no line
anywhere saying which of those are the *contract* and which are one
implementation of it.

`crates/forge-store/src/objectstore.rs` draws that line. It is a seam, not a
backend: `LocalBlobStore` remains the only production implementation, and no
network code ships with it.

## What the trait is

```rust
trait ObjectStore {
    fn durability_class(&self) -> DurabilityClass;
    fn begin_batch(&self) -> Box<dyn ObjectBatch + '_>;
    fn get(&self, id: ObjectId) -> Result<Vec<u8>>;   // re-hashes; I15
    fn has(&self, id: ObjectId) -> bool;              // visibility, not durability
    fn read_only(&self) -> bool;
    fn stats(&self) -> BlobStoreStats;                // barrier accounting
    fn put_parts(&self, parts: &[&[u8]]) -> Result<ObjectId>; // defaults to batch+finish
    fn put(&self, bytes: &[u8]) -> Result<ObjectId>;  // defaults to put_parts
}

trait ObjectBatch {
    fn put_parts(&mut self, parts: &[&[u8]]) -> Result<ObjectId>;
    fn put(&mut self, bytes: &[u8]) -> Result<ObjectId>;  // defaults to put_parts
    fn finish(self: Box<Self>) -> Result<()>;
}
```

`Store` is now `Store<O: ObjectStore = LocalBlobStore>`. The default type
parameter means the bare name `Store` still resolves to `Store<LocalBlobStore>`
in every existing call site, so nothing outside `forge-store` changed -- while
the compiler now proves `Store` uses nothing but the trait.

## I4 is the whole design problem

I4: *a committed ref implies fsynced object bytes and every directory edge
needed to reach them; visibility alone is never a durability proof.*

A three-method `put`/`get`/`has` trait cannot express that, and a backend
written against such a trait would weaken durability the first day it existed.
So the invariant is split into a half the trait enforces and a half it names.

### Enforced by the trait: ordering and completeness

Publication is two-phase, and both phases are in the signatures.

| Step | Guarantee on return |
|---|---|
| `ObjectBatch::put_parts` | the object *bytes* are durable. The object may be readable, but no ref may name it yet: the path reaching it can still be unproven. |
| `ObjectBatch::finish` | every object the batch published **or joined**, and every naming edge required to reach each one, is durable. This is the only point at which a caller may CAS-publish a ref naming those OIDs. |
| dropping a batch | nothing is published. No proof may be recorded. A crash here leaves durable orphan objects, which is safe. |

`LocalBlobStore` chooses *how* it takes the `finish` barriers with
`DirectoryBarrier` (`FORGEFS_DIR_BARRIER`): one `fsync` per touched directory
as it is touched, one per distinct directory in a single phase at `finish`
(the default), or one `syncfs(2)` for the whole batch shared with concurrent
batches. That is a backend implementation choice and not a seam contract: every
setting makes the same set of edges durable before `finish` returns, so the
table above is what a caller may rely on. `docs/BENCH.md` has the measured cost
of each and why the barrier-count winner is not the throughput winner.

"Joined" is the load-bearing word. If a batch deduplicates against an object
that some *other, unfinished* batch made visible, it inherits no proof at all
and must reproduce both barriers itself -- because the peer that made the
object visible may already be dead, while this batch's caller is about to
publish a ref. This is precisely the rule a naive trait loses silently, and it
is the first thing to check in any backend review.

`has` is a visibility predicate. Nothing on a ref-publishing path may use it as
a barrier; that is the exact confusion I4 forbids.

### Why the publishing primitive is a gather, not a buffer

`put_parts` publishes the object formed by concatenating its parts. It is the
required method and `put(bytes)` is the one-part default, rather than the other
way round.

That is not a convenience. #320 removed the two full-payload allocations
`put_blob_data` used to make: a publisher now hands the store `[frame_prefix,
payload]` and the payload is hashed and written where the caller already holds
it. A seam whose only publishing verb took `&[u8]` would force the concatenation
back into existence at the trait boundary and quietly undo that -- the seam would
have made a measured property of the system worse while claiming to change
nothing. `large_blob_memory.rs` is the gate that would catch it.

The trait therefore states the identity rule instead of hiding the shape:
`parts` is **one object cut anywhere**, never a structure inside the object. For
every split of the same byte string a backend must return the same ObjectId,
publish the same object file, and take the same barriers (I2, I3, I4).
`gather_addresses_the_concatenation` in the conformance suite asserts exactly
that, across every cut point, in both the single-object and the batched form,
and it runs against the in-memory backend too -- so the framing that
`put_blob_data` relies on is proven on a plane with no filesystem under it.

A backend that cannot write vectored concatenates *in its own body*, where the
cost is legible. `memory.rs` does exactly that and says so; what it may not do
is hash the parts separately or reframe them, because then the same repository
would hold two addresses for one byte string depending on how it was written.

### Not enforced by the trait: physics

No Rust signature can force an implementation to reach stable media. Whether
`finish` means `F_FULLFSYNC` on a leaf directory or a conditional `PUT`
acknowledged by a quorum is the implementation's business and stays there. To
stop that from becoming invisible, a backend must:

1. **Declare a `DurabilityClass`.** Only `CrashDurable` may back a repository
   that publishes refs. `ProcessLifetime` exists so a test or bench backend
   states its weakness in the API instead of in a comment.
2. **Pass the conformance suite unchanged**
   (`objectstore/conformance.rs`). It is backend-neutral: it asserts ordering,
   accounting and the join rule, and never an fsync count, a path or a syscall.
3. **Supply its own physical evidence**, which conformance cannot fabricate:
   a fault-injection point per barrier (`DurabilityBarrier`), a crash test that
   kills the process between barriers and shows no surviving ref names a
   non-durable object, and a cross-process test that a second process re-proves
   state a dead peer left merely visible. For the local backend those are
   `barrier_fault_injection.rs`, `sigkill_recovery.rs` and `cross_process_put.rs`;
   see `docs/RECOVERY.md`.

Point 3 is the honest gap: a review obligation, not a compile error. A reviewer
of any future backend should demand it by name and refuse the PR without it.

## Proving the abstraction is honest

`objectstore/memory.rs` is a second implementation with no filesystem at all.
It is `#[cfg(test)]`-only, so it cannot be linked into a release build. Both
backends run the same conformance suite, and `Store` itself is driven end to end
over the in-memory plane. If a local-filesystem assumption had leaked out of
`LocalBlobStore` into `Store`, that test would not compile.

## Deliberately left outside the trait

Two callers walk `objects/` directly through `Store::root`, and both are object
*enumeration*:

- `fsck`'s orphan sweep (`fsck.rs`, `scan_all_object_paths`);
- `gc --dry-run`'s candidate scan (`gc.rs`, `scan_objects`), which also reads
  each candidate's size and mtime for `--min-age-secs`.

Enumeration is a real capability a remote backend would have to provide (a
`list`/`scan` method returning at least an id, a size and an age), and
pretending otherwise inside the trait would be exactly the leak this seam
exists to prevent. Note what this costs today: `Store::root` is the repository
root, not the object plane's root, so a backend that does not keep its objects
under `<root>/objects` cannot run `fsck --full`'s orphan sweep or `gc` at all --
they are local-backend operations, not seam operations. That is the honest
statement of the gap, and closing it is the next backend author's first design
question, not a hidden bug.

The catalog is untouched. `Meta` is still local SQLite and the CAS ref
transaction is still the visibility point; this seam moves only the object
plane.
