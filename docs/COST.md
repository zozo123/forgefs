# ForgeFS cost model

ForgeFS optimizes measured bottlenecks, not folklore. `forge bench` is the source of machine-local numbers; this document says how to read them and which costs are allowed to motivate architecture.

## The checkin mix

A durable checkin crosses four cost classes:

1. content work: canonical encode + BLAKE3;
2. immutable storage: object write + file durability + parent-directory durability;
3. mutable metadata: process-local SQLite lock wait + `BEGIN IMMEDIATE` transaction/commit;
4. integration: ref CAS, fork publication, merge and seal when requested.

The existing benchmark exposes process-lifetime counters for storage and metadata next to p50/p95/p99 wall latency. They are deliberately counters, not a claim that overlapping concurrent work can be summed into one critical path.

| Event | Historical intuition | Measure here |
|---|---:|---|
| BLAKE3 / canonical encode | CPU-cheap relative to durable I/O | object/store counters + profile |
| durable object publication | barrier-bound | `puts`, `fsync_file`, `fsync_dir`, storage time |
| SQLite writer transaction | WAL commit + contention | `txn`, `txn_us`, `lock_wait_us`, `busy` |
| ref CAS | one serialized metadata decision | `cas_updated`, `cas_forked`, `cas_noop` |
| serial checkin | end-to-end | benchmark p50/p95/p99 |
| loaded private checkin | tail-at-scale | benchmark Hz + p50/p95/p99/max |

Do not copy microseconds from another machine into this table. Run:

```sh
cargo run --locked -p forge-cli -- bench --agents 32 --shared 16 --workers 16
```

and retain the complete output with the machine/filesystem description when making a performance claim.

## Current decision rule

A new cache, index, connection pool, batching lane, or storage layout must point to a measured counter or profile showing that its cost is material. Average throughput alone is insufficient: p99 and invariant evidence must hold. In particular, a durability optimization may coalesce barriers but may never acknowledge a committed ref before every object reachable from it is durable.

### Barriers: count the serialised critical path, not the barriers issued

An earlier form of this rule held that the per-checkin flush count essentially
*is* the serial cost, so removing a barrier buys its share of the average flush
time. That rule is refuted; it must not be reintroduced here.

- **Marginal is not average.** A barrier that follows other barriers finds the
  journal already committed. Measured on one Linux/ext4 box: roughly 49 us
  marginal against a roughly 402 us average. Multiplying an average flush cost
  by a flush count overstated the value of removing one barrier by about 8x.
- **The kernel already merges.** jbd2 coalesces concurrent fsyncs. At 16 workers
  ForgeFS *issued* 16.01 barriers per checkin while the device *completed*
  10.71. "Reduce the count" competes with a kernel that already does it, and
  loses outright when the replacement introduces a global serialisation point.
- **Both of this week's results, one model.** Collapsing 9 directory barriers to
  2 with I4 intact *lost* 15-22% throughput at 2..16 workers (#341); moving
  fsyncs out of the write mutex *won* (#338). A count-based rule predicts the
  opposite for both. What costs is a barrier on the serialised critical path,
  not a barrier issued.

State where a proposed change moves barriers *relative to the serialising mutex
or transaction*, then measure `flush_per_checkin` alongside issued-versus-
completed device flushes. A change that only lowers the issued count is not yet
evidence of a win.

### Two hazards that turn durability numbers into fiction

1. **Barrier reach is a precondition, not a detail.** A fresh sandbox may mount
   ext4 `nobarrier`, and every durability number taken there is fiction. Confirm
   the fsync-to-device-flush ratio (`/proc/diskstats` field 19) is about 1.00
   before trusting any figure, and remount with barriers if it is not. This is
   step zero of the bench command above, not an afterthought.
2. **`kill -9` cannot evidence power-loss durability.** SIGKILL leaves the page
   cache intact, so a clean kill test says nothing about whether barriers
   reached the device. Demonstrated rather than argued: a mutated build with a
   genuine I4 directory-edge hole passed a SIGKILL harness with 1,960
   acknowledged checkins, zero losses, and `fsck --full` clean on every run.
   SIGKILL evidences process-crash durability only; the power-loss half rests
   entirely on device flush counts plus a verified barrier-reach ratio.

## Cache policy

Caches are hints. The Store tree/blob LRU may avoid repeated decode/I/O on ordinary reads, but verification and fsck must remain able to re-read and validate durable bytes. Cache state is never content identity, authority, provenance, or evidence that a sealed object is sound.

## Reading the numbers

For a serial workload, compare wall time against storage durability time, SQLite transaction time, and local lock wait to identify the dominant class. For a concurrent workload, those counters overlap across workers: use them to locate amplification/convoys, not to manufacture an additive critical path. If accounted components differ materially from wall time, profile the unexplained remainder before redesigning the system.

This is the gate behind #37, #49, #140 and #177: measure first; change the smallest mechanism that moves the dominant cost; keep the immutable object format and correctness invariants stable unless the evidence requires otherwise.
