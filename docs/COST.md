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

## Cache policy

Caches are hints. The Store tree/blob LRU may avoid repeated decode/I/O on ordinary reads, but verification and fsck must remain able to re-read and validate durable bytes. Cache state is never content identity, authority, provenance, or evidence that a sealed object is sound.

## Reading the numbers

For a serial workload, compare wall time against storage durability time, SQLite transaction time, and local lock wait to identify the dominant class. For a concurrent workload, those counters overlap across workers: use them to locate amplification/convoys, not to manufacture an additive critical path. If accounted components differ materially from wall time, profile the unexplained remainder before redesigning the system.

This is the gate behind #37, #49, #140 and #177: measure first; change the smallest mechanism that moves the dominant cost; keep the immutable object format and correctness invariants stable unless the evidence requires otherwise.
