# Recovery and durability contract

ForgeFS has two persistence planes with different recovery rules. Do not reason about both as if they were one WAL.

| Plane | Publication rule | Recovery rule |
|---|---|---|
| Immutable objects (`.forge/objects`) | FORCE before reference publication: write a temporary file, sync the file, publish the content-addressed name, then sync the complete directory chain needed to reach it. On macOS these barriers use `F_FULLFSYNC`, matching the catalog's strength. Objects are write-once; an existing object name is never overwritten. | Unpublished temporary files may be reclaimed. A committed ref that names a missing or hash-invalid object is corruption, not a condition to repair silently. |
| Mutable catalog (`.forge/meta.sqlite`) | SQLite WAL with `synchronous=FULL`; on macOS ForgeFS also requires `fullfsync=ON`. Ref/session mutations use explicit transactions. | SQLite recovers committed WAL transactions. ForgeFS then validates its higher-level invariants; it does not reconstruct missing immutable objects from catalog rows. |

## Metadata durability policy

`Meta::open` establishes and verifies the catalog policy before migrations or metadata writes:

- `journal_mode=WAL`
- `synchronous=FULL` (`2`)
- macOS: `fullfsync=ON`
- a five-second connection busy timeout
- foreign-key checking enabled

Opening fails if the required WAL/synchronous/full-fsync contract cannot be established. ForgeFS therefore never knowingly continues under a weaker catalog setting.

The immutable object and bootstrap planes use the same platform policy: normal
file/directory `fsync` barriers on Linux and `F_FULLFSYNC` on macOS. A strong
SQLite WAL flush may never outrun a weaker object-publication flush.

A successful SQLite commit means SQLite has completed the durability work implied by those settings. That is a process/OS/filesystem contract, not a promise about hardware that lies about flushes or storage lacking stable power-loss semantics. On Linux, the guarantee ultimately depends on the filesystem and block device honoring SQLite's sync requests. On macOS, `fullfsync=ON` asks SQLite to use the stronger full-fsync path when supported.

## Ordering invariant

A ref must never become durable before every immutable object reachable from the new ref has been durably published. The ordering is:

1. canonical object bytes are written and hashed;
2. each new object file is synced;
3. each content-addressed name and required directory entry is durably published;
4. only then may the SQLite transaction publish the ref/session state;
5. the SQLite transaction commits under the policy above.

Group commit may amortize barriers, but it may not reorder steps 3 and 4.

Directory existence is not a durability proof. A newly opened Store re-proves
`objects -> aa -> bb -> OID` before joining a visible legacy or interrupted
publication. Within one process, bounded positive caches may reuse only a
directory edge whose parent barrier succeeded and an OID whose batch `finish`
completed. Cache eviction or process restart causes conservative re-proof; a
dropped batch never publishes a cache proof.

Repository initialization follows the same rule. Missing user path components
are created one edge at a time and their parents are forced; the `.forge` rename
is then forced in its parent. Every cold open re-proves `parent -> .forge`, so it
can safely recover an initializer that died after rename but before that final
barrier.

## What crashes may leave behind

A crash may leave durable but unreachable objects or temporary files. Those are safe because immutable object publication is idempotent and refs are the roots of truth. A crash must not leave a successfully committed ref pointing at a missing or corrupt object.

`forge fsck --full` is the end-to-end validator. It validates SQLite's physical structure and the mutable catalog's schema, reflog, seal, landmark, provenance, and namespace relations from one read snapshot before proving immutable object closure. Sealed provenance is exact: the signed manifest must cover the sealed content tree plus every causally reachable Contribution, and each receipt entry must agree with its immutable agent. If a catalog relation is inconsistent or a referenced object cannot be read and rehashed, recovery has failed and ForgeFS reports corruption rather than inventing bytes, moving refs, or silently repairing rows. A damaged migration ledger is admitted only to this detection-only fsck path so it can be named precisely; all other read-only and writable opens continue to fail closed on incompatible schema state.

## Executable fault matrix

Debug/test builds expose an explicitly armed, thread-local fault seam at each
durability transition. It has no environment trigger, global mutable plan, or
process-exit behavior, and its state and branches are compiled out of release
builds. The matrix covers failures of the real file and directory barriers as
well as interruptions immediately after file sync, object linking, directory
sync, and ref-transaction commit.

Failures through the final object-directory barrier must leave refs unchanged.
An interruption after the SQLite commit is different: the durable transaction
may have advanced the ref even though the caller did not receive its result.
That outcome is deliberately not rolled back or relabeled. A retry observes the
committed session state as a no-op, and a cold reopen plus full fsck must find
the exact committed ref and rehash its complete object graph.

The same suite exercises init staging, key, cleanup, parent, publication, and
cold-open publication barriers, orphan re-proof, and checkpoint bracketing.
This is deterministic state-machine evidence, not a physical power-loss claim;
real process-kill tests remain separate evidence for abrupt termination.

## Near-exhausted free space

A filesystem that cannot allocate is an availability failure, not a durability
failure, and ForgeFS treats it as one.

Below 32768 bytes free -- the size of the wal-index region SQLite maps for
`.forge/meta.sqlite` -- every command refuses to open the metadata and exits 5
rather than proceeding. The threshold is SQLite's own region size, not a tuning
choice: below it the mapping cannot be guaranteed to be backed by disk blocks,
and on a filesystem whose block size is smaller than the CPU page size a fault
into the unbacked remainder is delivered as SIGBUS. That kills the process with
no exit code and an empty stderr, so it must be prevented rather than reported.
CLI_ABI.md describes the caller-visible contract, including the residual window.

Such a kill does not put a repository at risk. It is an ordinary process death,
and the ordering invariant above already covers it: object bytes and their
directory edges are forced before any ref names them, so a ref that became
visible is durable whether the process exited or was killed. This was measured,
not assumed: thirteen SIGBUS kills taken mid-commit left `forge fsck --full`
clean over 160 objects with every committed file readable.

Recovery is therefore the same as for any crash. Free space on the filesystem
holding `.forge`, then run `forge fsck --full`. Objects written by the killed
operation but never named by a ref are unreachable, not corrupt, and are safe to
leave in place.

## Checkpointing and reopen

WAL checkpointing is not part of content identity and does not change ref semantics. A committed ref must survive an explicit SQLite WAL checkpoint and a fresh process/reopen unchanged. ForgeFS inspects the three-value checkpoint result and fails a busy or partial checkpoint instead of treating successful PRAGMA execution as proof of completion. Regression tests exercise commit -> checkpoint -> close -> reopen -> read-ref, in addition to barrier fault injection and real process-kill tests.

## Design references

The model follows the separation emphasized by ARIES/transaction-processing systems and SQLite WAL: database pages are recovered by the database WAL, while ForgeFS's immutable CAS uses its own force-before-reference publication invariant. See C. Mohan et al., *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging* (1992), Gray & Reuter, *Transaction Processing* (1993), and the SQLite WAL documentation.
