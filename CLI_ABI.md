# Forge CLI machine contract

Automation must key on exit codes, not stderr wording.

| Exit | Meaning |
|---:|---|
| 0 | success |
| 1 | denied/capability/input/not-found |
| 2 | corruption or sealed-state violation |
| 3 | transient busy/contention |
| 4 | stale observation or merge conflict |
| 5 | I/O, SQLite, or internal failure |

`--cap PATH|TOKEN` or `FORGE_CAP` is required for normal commands; ForgeFS has no ambient root authority.

## Termination without an exit code

The table above covers every failure ForgeFS can *report*. A caller must also
distinguish termination by signal, which carries no exit code at all. In a shell,
`$?` is then `128 + signum` (`135` for SIGBUS); with `waitpid` use `WIFSIGNALED`.
Treat that as its own outcome: it is not success, and it is not any row above.

One such case is known and is not a ForgeFS defect. ForgeFS keeps its metadata in
SQLite with `journal_mode=WAL`, so SQLite creates the wal-index
`.forge/meta.sqlite-shm`, sparse-extends it to 32768 bytes by writing one byte to
the last byte of each 4096-byte page, and then maps the whole region. Where the
filesystem block size is smaller than the CPU page size -- `mkfs.ext4` chooses
1024 or 2048 automatically for filesystems under about 512 MB -- each of those
one-byte writes allocates only the final block of its page, so a 32 KiB mapping
can be backed by as little as 8 KiB of blocks. A page fault into one of the
remaining holes on a filesystem that can no longer allocate is delivered as
SIGBUS, which is not catchable as an error return.

ForgeFS closes the reachable part of that window: every command checks free space
before SQLite can create the wal-index, and exits 5 with a diagnostic naming the
wal-index when fewer than 32768 bytes are available. Above that threshold SQLite's
own extension writes are all satisfied and the whole mapped region is backed by
the time the connection is usable, so no later fault can find a hole in it. A
residual window remains outside ForgeFS's control -- notably a wal-index that
grows past its first region inside a long operation -- so automation that runs
ForgeFS against a filesystem it does not control must still handle signal
termination. `forge fsck --full` is the correct response; see docs/RECOVERY.md.

A SIGBUS handler is deliberately not installed. The fault arrives on the thread
touching the mapping, inside SQLite, with SQLite's locks and its mapped wal-index
in an indeterminate state; returning from the handler retries the same faulting
access forever, and unwinding out of it would leave the wal-index and the WAL
writer half-updated. Refusing to enter the band is the only safe answer.

`scripts/enospc-sigbus-probe.sh` reproduces and asserts this contract. It needs
Linux, root and `mkfs.ext4`, so it is not part of `scripts/release-gate.sh`.

## Structured metrics: `forge stats --json`

`forge stats --json` writes exactly one JSON object to stdout and exits 0. It
introduces no exit code: a missing capability, an unreadable repository, or a
corrupt catalog are reported by the table above, unchanged.

The document is:

```
schema_version   integer, currently 1
scope            "process-lifetime"
note             prose restating scope
durability       journal_mode, synchronous, fullfsync, read_only
store            puts, dedup_hits, fsync_file, fsync_file_us,
                 fsync_dir, fsync_dir_us, barrier_us
sqlite           txn_count, txn_us, lock_acquires, lock_wait_us, busy,
                 cas_updated, cas_forked, cas_denied, cas_noop, accounted_us
api              sessions_opened, stale_observation, merge_applied, merge_conflict
```

Stability rules for consumers:

- Keys are added, never renamed or removed, while `schema_version` is 1. A
  consumer must ignore keys it does not know.
- Every counter is a non-negative integer. `barrier_us` and `accounted_us` are
  saturating sums of the components printed beside them, not wall time.
- **`scope` is the whole contract.** Every counter is a cumulative total for
  the single process that ran the command, from its repository open until the
  snapshot. It is not per-operation, not per-checkin, and not a benchmark. A
  one-shot `forge stats` therefore reports little more than its own open; the
  totals are meaningful for a long-lived embedder calling
  `Forge::stats_report()`, and the per-checkin cost mix remains unavailable
  (`docs/BENCH.md`).
- Values are environment-dependent. Automation may assert the shape; it must
  not assert amounts.

`forge stats` without `--json` renders the same numbers for humans and is not
part of this contract.

