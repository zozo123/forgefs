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

