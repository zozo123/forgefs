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

## Reclamation: `forge abandon` and `forge gc`

Neither verb introduces an exit code. Both map onto the table above:

| Outcome | Exit |
|---|---:|
| `abandon fork` retired the ref; `abandon session` retired the namespace | 0 |
| ref or namespace does not exist | 1 |
| ref is outside `forks/`, already abandoned, still mounted, still a session's live ref, or the session holds staged work without the explicit discard flag | 1 |
| ref is protected, or the capability may not write it | 1 |
| ref is sealed | 2 |
| `gc --dry-run` produced a report | 0 |
| `gc --collect` reclaimed (possibly zero) objects | 0 |
| `gc` with neither `--dry-run` nor `--collect`, or with both | 1 |
| `gc --collect --min-age-secs` below the hard floor | 1 |
| `gc` under a ref-scoped capability | 1 |
| `gc` could not prove reachability because an object is unreadable or does not decode | 2 |

`forge gc --dry-run` **never deletes** and is the reporting half. `forge gc
--collect` is the reclaiming half: it unlinks unreachable objects and removes
the catalog rows that named them. Exactly one of the two flags is required, so
a bare `forge gc` still exits 1 with the diagnostic pointing at `docs/GC.md`,
and no invocation deletes anything by default. `--min-age-secs` is refused
below its hard floor rather than quietly raised, because that floor is the only
bound ForgeFS has on the window between a writer's put and the transaction that
names it. `docs/GC.md` states the root set, the invariant collection preserves
(I19) and the one precondition it cannot prove for itself.

`forge gc --json` writes one JSON object to stdout. It is not part of the
`forge stats --json` contract above and carries no `schema_version`; consumers
must ignore keys they do not know and must not assert amounts.

## Structured metrics: `forge stats --json`

`forge stats --json` writes exactly one JSON object to stdout and exits 0. It
introduces no exit code: a missing capability, an unreadable repository, or a
corrupt catalog are reported by the table above, unchanged.

The document is:

```
schema_version   integer, currently 2
scope            "process-lifetime"
note             prose restating scope
durability       journal_mode, synchronous, fullfsync, read_only
store            puts, dedup_hits, fsync_file, fsync_file_us,
                 fsync_dir, fsync_dir_us, barrier_us
sqlite           txn_count, txn_us, explicit_txn_count, lock_acquires,
                 lock_wait_us, write_lock_acquires, write_lock_wait_us,
                 read_lock_acquires, read_lock_wait_us, busy, cas_updated,
                 cas_forked, cas_denied, cas_noop, accounted_us
api              sessions_opened, stale_observation, merge_applied, merge_conflict
```

Stability rules for consumers:

- Keys are added, never renamed or removed, while `schema_version` is 2. A
  consumer must ignore keys it does not know.
- `txn_count` is every write transaction SQLite committed on the catalog: each
  explicit `BEGIN IMMEDIATE` that committed, and each autocommit statement that
  wrote, since SQLite gives every such statement its own implicit transaction.
  `explicit_txn_count` is the explicit half alone and is the only sample count
  that pairs with `txn_us`; `txn_count / txn_us` is not an average.
- `lock_acquires` / `lock_wait_us` sum the write connection's mutex and the
  read pool's slot mutexes, so they measure neither family on its own. Use
  `write_lock_acquires` / `write_lock_wait_us` for writer contention and
  `read_lock_acquires` / `read_lock_wait_us` for the pool.
- **Schema 1 -> 2 (issue #311).** No key was renamed or removed, but under
  schema 1 `txn_count` counted only explicit transactions, so a read-heavy
  phase reported `0` while the catalog committed one autocommit write per
  operation. A consumer comparing a schema-1 series to a schema-2 series is
  comparing two different quantities; `explicit_txn_count` is the field that
  continues the old series.
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

