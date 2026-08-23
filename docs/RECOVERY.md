# Recovery and durability contract

ForgeFS has two persistence planes with different recovery rules. Do not reason about both as if they were one WAL.

| Plane | Publication rule | Recovery rule |
|---|---|---|
| Immutable objects (`.forge/objects`) | FORCE before reference publication: write a temporary file, sync the file, publish the content-addressed name, then sync the containing directory. Objects are write-once; an existing object name is never overwritten. | Unpublished temporary files may be reclaimed. A committed ref that names a missing or hash-invalid object is corruption, not a condition to repair silently. |
| Mutable catalog (`.forge/meta.sqlite`) | SQLite WAL with `synchronous=FULL`; on macOS ForgeFS also requires `fullfsync=ON`. Ref/session mutations use explicit transactions. | SQLite recovers committed WAL transactions. ForgeFS then validates its higher-level invariants; it does not reconstruct missing immutable objects from catalog rows. |

## Metadata durability policy

`Meta::open` establishes and verifies the catalog policy before migrations or metadata writes:

- `journal_mode=WAL`
- `synchronous=FULL` (`2`)
- macOS: `fullfsync=ON`
- a five-second connection busy timeout
- foreign-key checking enabled

Opening fails if the required WAL/synchronous/full-fsync contract cannot be established. ForgeFS therefore never knowingly continues under a weaker catalog setting.

A successful SQLite commit means SQLite has completed the durability work implied by those settings. That is a process/OS/filesystem contract, not a promise about hardware that lies about flushes or storage lacking stable power-loss semantics. On Linux, the guarantee ultimately depends on the filesystem and block device honoring SQLite's sync requests. On macOS, `fullfsync=ON` asks SQLite to use the stronger full-fsync path when supported.

## Ordering invariant

A ref must never become durable before every immutable object reachable from the new ref has been durably published. The ordering is:

1. canonical object bytes are written and hashed;
2. each new object file is synced;
3. each content-addressed name and required directory entry is durably published;
4. only then may the SQLite transaction publish the ref/session state;
5. the SQLite transaction commits under the policy above.

Group commit may amortize barriers, but it may not reorder steps 3 and 4.

## What crashes may leave behind

A crash may leave durable but unreachable objects or temporary files. Those are safe because immutable object publication is idempotent and refs are the roots of truth. A crash must not leave a successfully committed ref pointing at a missing or corrupt object.

`forge fsck --full` is the end-to-end validator. If a referenced object cannot be read and rehashed, recovery has failed and ForgeFS reports corruption rather than inventing bytes or silently moving refs.

## Checkpointing and reopen

WAL checkpointing is not part of content identity and does not change ref semantics. A committed ref must survive an explicit SQLite WAL checkpoint and a fresh process/reopen unchanged. Regression tests should exercise commit -> checkpoint -> close -> reopen -> read-ref, in addition to barrier fault injection and real process-kill tests.

## Design references

The model follows the separation emphasized by ARIES/transaction-processing systems and SQLite WAL: database pages are recovered by the database WAL, while ForgeFS's immutable CAS uses its own force-before-reference publication invariant. See C. Mohan et al., *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging* (1992), Gray & Reuter, *Transaction Processing* (1993), and the SQLite WAL documentation.
