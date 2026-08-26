# Replication: the object plane and the ref plane

Owns the decision on issue #25. `docs/OBJECTSTORE.md` owns the single-node
`ObjectStore` contract; this document owns what happens when there is more than
one node, and why there is not one yet.

Status: **research, not scheduled.** The stated blocker has cleared, the
architectural split is sound and unchanged, and the first PR is now identifiable
and small. It is not distribution; it is closing a seam leak that distribution
would otherwise discover the hard way.

## 1. The split, restated

#25 proposes two planes with different consistency stories, and that is correct:

| Plane | Contents | Consistency |
|---|---|---|
| object | immutable, content-addressed objects under `objects/` | eventually consistent; pull/push missing OIDs by Merkle traversal; idempotent and deduplicating by construction |
| ref | `meta.sqlite` -- refs, reflog, sessions, mounts, pins, observations | linearizable within one cell; **not replicated** |

Helland's rule, quoted in the issue thread: replicate outside, not inside. A
`.forge` directory is one Hamilton cell and one blast radius. Replicating
`meta.sqlite` would mean two catalogs disagreeing about which ref holds which
commit, which is the one thing CAS publication exists to prevent.

Nothing in the last year has weakened that. I3 (put is idempotent iff bytes
match) and I2 (one logical object, one byte string, one ObjectId) mean the
object plane converges without coordination: transfer is set union, retries are
free, and a duplicate arrival is a no-op. That is the property a Merkle DAG
gives you for nothing, and it is worth taking.

The ref plane genuinely is a separate problem, and the issue is right not to
guess at it. Single integrator, a consensus service, and append-only signed ref
events are all defensible; which one is correct depends on a deployment nobody
has yet.

## 2. What has changed since the issue was written

The issue thread says **blocked by #143**: freeze the node contract before any
WAN. #143 is closed. The seam exists:

- `crates/forge-store/src/objectstore.rs` defines `trait ObjectStore`
  (`durability_class`, `begin_batch`, `get`, `has`, `read_only`, `stats`) and
  `trait ObjectBatch` (`put_parts`, `put`, `finish`).
- `Store` is generic over it with `LocalBlobStore` as the default parameter.
- `objectstore/memory.rs` is a second implementation with no filesystem, and
  both run the same backend-neutral conformance suite.
- `docs/OBJECTSTORE.md` records the I4 two-phase rule a backend must honour and
  the physical evidence a backend author must supply.

So the precondition #25 named is met. The gate in the issue body -- "no
distributed mode until local invariants and fsck are solid" -- is **not** met.
`#359` is closed and only half of what it names is fixed: fsck and gc no longer
call a repository past 1,000,000 objects CORRUPT -- they refuse the walk, exit
1, and the ceiling is now settable -- but the walk itself is still not
incremental or resumable, and that is the half a second node needs. `#21`'s V1
gate 1 is also open. Truth at scale on one node comes first. This issue stays P2
research.

## 3. The first PR: enumeration is not on the seam

`docs/OBJECTSTORE.md` already names this as the honest gap, and it is exactly
the thing a second node needs first. Two callers reach around the trait and read
the local filesystem directly:

```
crates/forge-api/src/fsck.rs:580   fn scan_all_object_paths(root: &Path, ...)
                                     fs::read_dir(root.join("objects"))
crates/forge-api/src/gc.rs:784     fn scan_objects(root: &Path, ...)
                                     fs::read_dir(root.join("objects"))
```

`Store::root` is the repository root, not the object plane's root. The
consequence, stated plainly: **a backend that does not keep its objects under
`<root>/objects` cannot run `fsck --full`'s orphan sweep or `gc` at all.** Those
are local-backend operations today, not seam operations.

That matters for replication specifically, because Merkle pull is enumeration in
disguise. "Which objects do you have that I do not" is answered either by
walking the DAG from a ref (which the seam supports today via `get`) or by
listing (which it does not). Reachability-driven pull is the better protocol
anyway -- it transfers only what a named commit needs, and it never depends on a
peer's ability to list a bucket -- but `gc` and `fsck --full` genuinely need
enumeration, and a replicated repository still has to be collectable and
auditable.

**Proposed first PR (single node, no network):** add a `scan` capability to the
`ObjectStore` seam returning at least `(ObjectId, size, age)` per object, port
`scan_all_object_paths` and `scan_objects` onto it, and prove it against
`objectstore/memory.rs` -- which today cannot be `fsck`ed or `gc`ed at all.
Success criterion: `fsck --full` and `gc` pass their existing tests driven over
the in-memory backend. No new invariant, no protocol, no second process.

Note the ordering hazard: I23 makes collection sound by re-reading each
candidate's age under the object plane's exclusive lock, so `scan` must expose
age in a way a backend can keep honest under that lock, not as a cached
attribute. That constraint belongs in the seam's rustdoc, and it is the reason
this PR is design work rather than a mechanical move.

## 4. What a replication design would then have to settle

Recorded so the next person does not rediscover them:

1. **Reachability pull, not list-diff.** Walk from a ref's commit; request the
   objects a peer lacks; verify each by hash on arrival (I15 -- a
   content-addressed read that trusts the sender is not a read). Corrupt is not
   a retry.
2. **Provenance travels or it does not.** A seal manifest names every reachable
   Tree, Blob and Contribution with an attribution string, and I15 makes a
   missing or extra key corruption. A partial pull that satisfies a commit but
   not its manifest cannot be sealed on the receiving node. Decide whether
   replication transfers manifests or re-derives them.
3. **The `intro` table is catalog, not object.** First-introducer attributions
   live in `meta.sqlite`, so a pulled object arrives with no attribution and
   `seal` on the receiver falls back to `"unknown"`. This is the sharpest
   inside/outside seam in the system and #25 does not currently mention it.
4. **Capabilities do not cross cells.** I14 says a namespace ID is not a
   capability, and `cli_cross_cell.rs` proves a token from one forge is refused
   by another. Any transport needs its own authority story; a macaroon is scoped
   to the cell that minted it.
5. **GC across replicas.** I23's soundness argument is single-cell: a pin is a
   catalog row, so it is a root. A replica has no view of another replica's
   pins, so either objects are never collected on a replica or roots are
   exchanged. This is the hardest unsolved piece and it should be settled on
   paper before any transfer code exists.

## 5. Scale

Multi-week to multi-month, and correctly parked. The enumeration PR in section 3
is days and pays for itself on one node -- it makes the in-memory backend
`fsck`able and closes a documented seam leak -- so it should be done on its own
merits, not as the start of a replication project.

Items 2 through 5 of section 4 are each a design document in their own right.
None of them should begin before a resumable graph walk -- the half of `#359`
its classification fix deliberately left open -- and `#21`'s gate 1.
