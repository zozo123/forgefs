### W7 -- ForgeFS vs Git worktrees

Precondition, storage and barrier reach (docs/BENCH.md). All four
configurations run under one directory tree, so every row below is on
the same filesystem:

```text
work directory:   /workspace/w7-final/work
filesystem:       ext4 on /dev/vdd (rw,relatime,discard)
fsync probe:      200 fsyncs moved device flush count by 200
barrier reach:    reaching
```

Workload W7 (W1 logical shape): 32 agents, 4 workers, one small edit and
one commit per agent onto its own ref, 9 fresh-repository repetitions per
configuration. Medians are run-level medians (median_low).

| Configuration | ops/s | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|
| ForgeFS (synchronous=FULL, file+dir fsync) | 245.8 | 15.08 | 19.12 | 20.61 | 20.61 |
| ForgeFS through the `forge` CLI, same durability (3 execs/agent) | 131.4 | 27.12 | 40.09 | 57.97 | 57.97 |
| Git worktrees, as-shipped default | 344.9 | 10.65 | 15.79 | 18.10 | 18.10 |
| Git worktrees, core.fsync=all fsyncMethod=fsync | 254.7 | 14.06 | 20.21 | 23.34 | 23.34 |

Observed durability barriers for one agent operation:

| Path | file fsync | dir fsync |
|---|---|---|
| ForgeFS write+checkin | 6 | 20 |
| Git add+commit, default | 0 | 0 |
| Git add+commit, durable | 6 | 0 |

Durability-equivalence gate (docs/BENCH.md W7):

- Git default: **non-comparable: durability mismatch/unknown** -- durability mismatch: ForgeFS issues 6 file barrier(s) for the measured operation and Git issues 0, so Git is not persisting what ForgeFS persists. The two ops/s numbers above stand on their own; their quotient is not a speed ratio.
- Git durable: **non-comparable: durability mismatch/unknown** -- durability mismatch: ForgeFS issues 20 dir barrier(s) for the measured operation and Git issues 0, so Git is not persisting what ForgeFS persists. The two ops/s numbers above stand on their own; their quotient is not a speed ratio.

Process-spawn floor (same driver, `git rev-parse --verify HEAD` per agent): 1176.1 ops/s, p50 2.99 ms. Each Git agent operation costs two more execs than that; ForgeFS W1 runs in-process threads. Subtract this before attributing any part of the gap to storage design.

Process-model note. `forge bench` drives its agents as in-process threads while Git execs twice per agent. The `forge` CLI row above runs the same ForgeFS work with three execs per agent and is the row to compare against Git if your agents shell out. At 4 workers the no-op Git exec floor alone is 1176.1 ops/s, so any Git deficit smaller than that floor is process-model cost and not storage design.

Excluded from every timed region on both sides: repository creation, and per agent worktree/branch creation (Git, median 0.562 s for all agents) versus capability grant and session open (ForgeFS, inside the W1 timed region).

Correctness gate: ForgeFS `fsck --full` inside `forge bench`; Git `fsck --strict` plus a per-agent check that every branch carries exactly one new commit with the exact bytes. All runs passed.

Environment line:

```text
forgefs commit:        5a7017a4d5e0fd156a456b342ca294dcbb44cb57
build profile:         release
forge --version:       forge 0.3.0
rustc:                 rustc 1.97.0 (2d8144b78 2026-07-07)
command line:          /workspace/forgefs/target/release/forge bench --agents 32 --shared 0 --workers 4 ;; w7_git_worktree_bench.py run --agents 32 --workers 4 --durability {default,durable}
worker count:          4
cpu model:             AMD EPYC 9454 48-Core Processor
cpu logical cores:     4
ram:                   7.8 GiB (8343212032 bytes)
os:                    Debian GNU/Linux 12
kernel:                Linux 6.16.9+
arch:                  x86_64
filesystem:            ext2/ext3
storage device:        /dev/vdd[/workspace]
sqlite journal_mode:   WAL
sqlite synchronous:    FULL (declared; Meta::open fails closed without it, docs/RECOVERY.md)
macos fullfsync:       n/a (Linux fsync path, docs/RECOVERY.md)
run class:             cold
repetition:            1
repository class:      fresh repository per repetition, both sides
```

ForgeFS lifetime counters, repetition 1 (whole-process totals; never divide by the checkin count):

```text
durability       journal_mode=wal synchronous=FULL(2) fullfsync=n/a
storage lifetime puts=241 bytes=unavailable fsync_file=241 fsync_file_us=132666 fsync_dir=643 fsync_dir_us=239561 barrier_fs=0 barrier_fs_us=0 barrier_fs_batches=0 lifetime_barrier_us=372227
sqlite lifetime  lock_acquires=649 lock_wait_us=47453 txn_count=148 explicit_txn_count=148 txn_us=112045 lifetime_accounted_us=159498 busy=0 updated=80 forked=0 denied=0
```

Git configuration observed in the measured repository:

```text
git version 2.39.5
default: (no fsync-related config set)
durable: file:.git/config	core.fsync=all
durable: file:.git/config	core.fsyncmethod=fsync
```
