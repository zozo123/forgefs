# Power-loss durability evidence for I4

> I4 — A committed ref implies fsynced object bytes and every directory edge
> needed to reach them; visibility alone is never a durability proof.

This document states exactly what is now proven about I4 under **power loss**,
by what mechanism, and what remains unproven. Durability evidence is precisely
what people stop re-checking once someone says it is handled, so the limits
below are part of the claim, not a footnote to it.

## The gap this closes

Every crash test in this repository used `kill -9`. SIGKILL destroys the
**process** and leaves the **page cache** intact, so the filesystem afterwards
still contains writes that never reached the device. The whole SIGKILL suite is
therefore blind to the exact defect class I4 exists to prevent: a ref published
over bytes or directory edges that no barrier ever forced.

That is measured, not argued. With a knowingly broken build — the
`ensure_path_dir` mutation described under "Validation" — `dir-barrier-sigkill.sh`
reports:

```
run=1 policy=deferred writers=6 acked=1197 fsck=clean lost=0
run=2 policy=deferred writers=6 acked=1003 fsck=clean lost=0
SIGKILL SUMMARY policy=deferred runs=2 acknowledged_checkins=2200 lost=0
```

2,200 acknowledged checkins, zero losses, against a build whose own unit tests
fail. The SIGKILL harness cannot see this and structurally never could.

## Why the kernel route is unavailable here

The authoritative technique is block-layer fault injection: `dm-log-writes`
records the exact write/flush/FUA stream and replays the volume to any barrier
point, or `dm-flakey` with `drop_writes`. It was tried first and is impossible
on this kernel, established rather than assumed:

| Probe | Result |
|---|---|
| `sudo` to root | available (uid 0) |
| `/dev/mapper/control`, `/dev/loop-control` | present; dm and loop are live |
| `dmsetup targets` | `multipath`, `striped`, `linear`, `error` only |
| `zcat /proc/config.gz` | `# CONFIG_DM_LOG_WRITES is not set`, `# CONFIG_DM_FLAKEY is not set`, `# CONFIG_DM_DELAY is not set`, `# CONFIG_DM_SNAPSHOT is not set` |
| `CONFIG_MODULES` | `# CONFIG_MODULES is not set` |
| `/lib/modules` | absent |

The targets are compiled out **and** the kernel is monolithic, so no module can
supply them. There is no route to `dm-log-writes` on this machine. If you run
this harness on a kernel that has those targets, prefer them: replaying to each
recorded flush point is strictly stronger than what follows.

## What was built instead

`scripts/powerloss/` implements the same idea one layer up.

**`pl_shim.c`** — an `LD_PRELOAD` interposer that records an ordered journal of
the durability-relevant libc calls of every process in a workload: `write`,
`pwrite`, `pwrite64`, `writev`, `pwritev`, `fsync`, `fdatasync`, `syncfs`,
`sync_file_range`, `msync`, `mmap`/`mmap64`, `rename`/`renameat`/`renameat2`,
`link`/`linkat`, `symlink`, `unlink`/`unlinkat`, `mkdir`/`mkdirat`, `rmdir`,
`ftruncate`/`truncate`, `chmod`/`fchmod`/`fchmodat`, `open`/`openat` (including
`O_CREAT`, `O_TRUNC`, `O_SYNC`, `O_DSYNC`), `close` and the `dup` family.
Records from different processes merge into one total order through an mmapped
atomic counter, and the payload bytes of every write are kept so the image can
be reconstructed exactly.

**`pl_replay.py`** — reconstructs what a **device** would hold after a cut at a
chosen point, under an adversarial model:

* file bytes are durable only if a completed `fsync`/`fdatasync` covered them
  (or they were written to an `O_SYNC` fd);
* a **directory edge** — create, link, rename, unlink, mkdir, rmdir — is
  durable only if the **parent directory** was fsynced, never because the file
  itself was;
* everything else is **dropped**. Dropping is exactly what SIGKILL fails to do,
  and it is the whole value of the harness.

Directories are modelled with real subtree semantics, so `init`'s
stage-then-rename is reproduced rather than approximated.

**The postcondition is I4 itself.** A ref that survives the cut is a *committed*
ref, so `forge fsck` — which roots the metadata and walks every object reachable
from it — must find all of its bytes and edges. In `MODE=cli` an acknowledgement
log is written into the same ordered stream (by `pl_mark.c`, see "stdio" below),
so a ref that was *promised* and then lost is named individually.

### Workload modes, and why more than one is required

| Mode | Shape | Reaches |
|---|---|---|
| `cli` | many short-lived `forge` processes, ack-checked | the object/metadata barrier ordering of a single batch |
| `bench` | one process, concurrent batches, `init` **inside** the trace and an **empty** baseline | bootstrap durability; nothing is durable by assumption |
| `multibatch` | one process, many sequential batches **including dropped ones** | the process-local durability caches |

`multibatch` exists because of a fact that took a wrong answer to find: every
`forge` CLI invocation is one process running one publish batch, so a CLI
workload **cannot exercise `durable_dirs`/`durable_oids` at all**. Those caches
span batches within a process, which is the shape the daemon (`forge serve`) and
every library embedding actually have. The workload uses the pattern the product
itself advises: a session with work staged under one of two read-write mounts
gets an I22 refusal from `checkin` on the empty mount — the refusal has already
folded the other mount into a publish batch it now drops — and the caller then
does what the message says and checks the mount in on its own.

## Validation

### The decisive test: it must fail on a known-broken build

The mutation: in `PublishBatch::ensure_path_dir`, record the deferred directory
proof at `mkdir` time instead of after its barrier, so a later batch skips a
barrier for an edge that was never durable.

| Build | Harness | Result |
|---|---|---|
| mutated | `dir-barrier-sigkill.sh` | 2,200 acked checkins, **lost=0**, `fsck --full` clean — blind |
| mutated | power-loss, `MODE=multibatch` | **VIOLATION at 18 of 18 cut points** |
| mutated | power-loss, `MODE=cli` | clean — see below |
| clean | power-loss, `MODE=multibatch` | **CLEAN**, 150 acks replayed |

A sample violation names the ref, the commit, and the edge:

```
VIOLATION fsck (ref-rooted) failed rc=2: FAILED (reachable): 391 refs, 619 objects
  [OBJECT_READ] commit:0068...cee:tree: 8605...43b: not found: object 8605...43b
  .../objects/86/05/8605...43b
    leaf entry durable, but ANCESTOR EDGE .../objects/86 never reached the
    device (parent .../objects holds 3 unbarriered edge(s))
```

**`MODE=cli` reports the mutated build clean, and that is reported rather than
tuned away.** The defect is genuinely unreachable from a one-batch-per-process
workload: the poisoned proof lives in a process-local LRU that a CLI process
discards at exit. The repository's own unit tests agree — the two tests the
mutation breaks are `deferred_unfinished_batch_publishes_no_directory_proof` and
`collapsed_unfinished_batch_publishes_no_directory_proof`, both of which require
an **unfinished** batch. The harness catches the defect in the mode where the
defect exists, and says so in the mode where it does not.

### The converse: it must be clean on correct code

Unmutated `main`, all CLEAN, every cut point checked with a cold `forge fsck`:

| Mode | Policy | Cuts | Acks replayed |
|---|---|---|---|
| cli | per-directory | 15 | 689 |
| cli | deferred | 15 | 418 |
| cli | collapsed | 15 | 329 |
| multibatch | per-directory | 15 | 150 |
| multibatch | deferred | 15 | 150 |
| multibatch | collapsed | 15 | 150 |
| bench | deferred | 12 | — (ref-rooted only) |
| cli | deferred, `PARTIAL=1` | 15 | 420 |
| multibatch | deferred, `PARTIAL=1` | 18 | 150 |

### A second, independent mutation

Devised separately from the first: in `PublishBatch::finish`, fsync the object
**file** but drop the barrier on the directory that names it. `MODE=cli`
reports **VIOLATION at 12 of 12 cut points**, and attributes it correctly and
differently — `leaf entry not durable, every ancestor durable`, the opposite
shape from mutation 1. The same SIGKILL run reports 1,167 acked checkins and
`lost=0`. A harness that distinguishes two failure shapes it was not tuned for
is a durability test, not a regression test for one bug.

### `PARTIAL=1` — a torn fsync, and a modelling trap

`PARTIAL=1` models a cut *during* the fsync at the cut point: a prefix of the
effects it was covering reached the device and the call never returned. The
first implementation tore **every** fsync in the stream and discarded whatever
it did not apply; that fired on correct code, because a returned `fsync` did
cover everything and a later `fsync` must still be able to flush what an earlier
one left dirty. The tear is now applied only to the image at the cut and then
undone. Recorded here because a harness that reports violations on correct code
is worse than useless — it is believable.

Note that plain **reordering** of un-barriered writes adds nothing to this
model: every un-barriered effect is already dropped, which is strictly stronger
than reordering it. Tearing matters only at the cut, where it is modelled.

## Blind spots, measured rather than assumed

`scripts/powerloss/pl-interpose-audit.sh` runs one identical checkin twice — once
under `strace` (kernel boundary), once under the shim (libc boundary) — and
prints both counts. Current result:

```
operation                kernel       shim   verdict
write                        72         72   covered
fsync                        32         32   covered
link                          4          4   covered
unlink                        4          4   covered
mkdir                         8          8   covered
ftruncate                     3          3   covered
mmap_shared_write             3          3   OBSERVED-NOT-FOLLOWED (stores bypass libc)
shared-writable mappings cover: meta.sqlite-shm
AUDIT OK
```

* **Dynamic linking**: confirmed. `forge` links `libc.so.6`; the harness refuses
  to run against a binary that does not.
* **Direct `syscall()`**: none found. Rust `std` and SQLite both reach libc
  through the PLT, and every kernel-side count matches the shim exactly.
* **`copy_file_range`, `sendfile`, `io_uring`**: not used; the audit fails if
  they appear.
* **mmap** — the one real hole, and it is bounded. Stores into a shared writable
  mapping happen in userspace with no syscall, so the shim cannot follow them.
  The audit reports it as a count and names the files: the only such mapping is
  `meta.sqlite-shm`, the SQLite **wal-index**, which is a rebuildable cache — the
  first connection after a crash re-derives it from the `-wal`. The replay
  deletes it, and every replayed image recovers and answers `refs`/`fsck`
  correctly, which is the verification, not the assumption. The audit fails if
  any mapping ever covers something else, such as the database itself or an
  object file.
* **glibc stdio** — found the hard way and worth stating: a shell builtin's
  redirected output flushes through a libc-**internal** alias of `write` that no
  `LD_PRELOAD` object can interpose. The first acknowledgement log, written with
  `printf >> file`, was silently invisible. `pl_mark.c` now appends each
  acknowledgement with one direct `write(2)`. Any future workload that writes
  through `FILE*` would be invisible to this harness in the same way.

## A finding about `fsck --full` after a crash

`dir-barrier-sigkill.sh` asserts `fsck --full` clean after every kill. That
assertion holds only because the page cache preserves the whole publish batch.
Under real power loss it is **not** a valid postcondition, and this harness
observed it failing on unmutated `main`:

a batch's directory barriers are taken per shard, so a cut inside `finish()` can
leave a commit object's own edge durable one barrier before the edge of a tree it
names. `fsck --full` roots **orphan object files** by design, walks that
unreferenced commit, and reports the missing tree. Nothing points at that commit;
GC reclaims it; `RECOVERY.md` already says the crash contract is ref-rooted —
"a crash must not leave a successfully committed ref pointing at a missing or
corrupt object". So the harness asserts ref-rooted `forge fsck` and reports
`fsck --full` outcomes as `orphan_partial_batch`, informational and never a
violation. This is a correction to what the crash suite claims, not a defect in
publication ordering.

## Running it

```bash
cargo build --locked -p forge-cli
cargo build --locked -p forge-api --example powerloss_multibatch

# CI-sized (what ci.yml runs)
scripts/powerloss/power-loss-gate.sh target/debug/forge

# much wider, on demand
CUTS=64 PL_SECONDS=15 ITERS=2000 PARTIAL=1 \
  scripts/powerloss/power-loss-gate.sh target/debug/forge

# one mode, one policy
MODE=multibatch POL=collapsed CUTS=40 ITERS=1000 \
  scripts/powerloss/power-loss.sh target/debug/forge
```

Nothing here runs in `cargo test`; the harness is shell plus one example binary,
so the ordinary suite is unchanged. It skips cleanly on non-Linux and without a
C compiler, `python3`, or `strace`.

## What this does NOT prove

The evidence stops here, and it stops well short of "power loss is handled".

* **Firmware that lies about FLUSH.** Everything above is a statement about
  what the application asked the kernel for. A device that acknowledges FLUSH
  without committing to stable media defeats all of it, and nothing in
  userspace can detect that.
* **A volatile write cache that ignores FUA**, or a disk that reorders across a
  cache flush. Not observable from here.
* **Torn sectors below the modelled granularity.** The model tears at write
  granularity at the cut point, not at 512-byte or 4 KiB device sectors, and it
  does not model a partially written sector inside a single `write`.
* **The kernel and filesystem themselves.** This models the POSIX contract, not
  ext4/XFS/btrfs. It is *more* adversarial than a real journalling filesystem —
  ext4's shared journal makes many missing directory barriers invisible in
  practice, which is the reason I4 must not depend on it — but a filesystem bug
  is outside what any userspace replay can see.
* **Stores into shared writable mappings**, as measured above. Bounded today to
  the rebuildable SQLite wal-index; the audit fails if that changes.
* **Anything a process does that does not go through libc.** No direct
  `syscall()`, `io_uring`, or `copy_file_range` was found in this workload, and
  the audit fails if any appears — but the audit only covers the paths the
  workload actually exercises.
* **macOS.** `F_FULLFSYNC` is on the production path (see `RECOVERY.md`) and is
  not exercised here; the harness is Linux-only.
* **Cut-point coverage is sampled, not exhaustive.** The default gate checks a
  handful of points; wide runs check dozens. A defect that only manifests in an
  unsampled window will be missed, and the sampled points are seeded and
  reproducible rather than adversarially chosen.
