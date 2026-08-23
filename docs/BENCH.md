# ForgeFS benchmark protocol

Performance claims must name a workload ID, hardware, durability policy, and raw result source. A single ops/s number is not sufficient.

## Workloads

| ID | Workload | Independent variable | Correctness condition |
|---|---|---|---|
| W1 | Private agents, disjoint one-line files | agents: 1, 8, 32, 128, 512, 1k, 10k where practical | every checkin is `Updated`; final `fsck --full` is clean |
| W2 | Shared-ref stampede | writers: 2, 8, 32, 128, 1k | exactly one `Updated`; all other writers `Forked`; `fsck --full` clean |
| W3 | Stale observation | two agents | stale reader fails deterministically; no silent commit |
| W4 | Same-path overlap | two agents | conflict is explicit and both immutable inputs remain reachable |
| W5 | Crash and reopen | kill/fault phase | committed refs never dangle; reopen plus `fsck --full` is valid or corruption is reported loudly |
| W6 | Large tree walk/update | entries: 10k, 100k, 1M | output is identical; report lookup/update scaling |
| W7 | Git worktree comparison | W1 only | same logical workload and durability intent; publish scripts and raw output |

`forge bench` should cover W1 and W2. W3-W5 belong in deterministic e2e/crash tests. W6 is a scaling study. W7 is the external comparator.

## Required environment line

Every published run records:

- ForgeFS commit SHA and build profile;
- CPU model and logical-core count;
- RAM;
- OS and kernel/version;
- filesystem (for example ext4 or APFS);
- physical/storage device description when known;
- SQLite `journal_mode` and `synchronous` settings;
- whether macOS `fullfsync` is enabled;
- whether the run is cold, warm, or both.

Do not compare results from different durability policies as if they were the same system.

## Required metrics

For each concurrency point report at minimum:

- throughput (ops/s);
- latency p50, p95, p99, and max;
- successful `Updated`, `Forked`, `Noop`, stale, and conflict counts as applicable;
- SQLite busy/wait and transaction time when instrumented;
- object puts and bytes;
- file-fsync and directory-fsync counts/time when instrumented;
- CPU and peak RSS for long/large runs;
- final `fsck --full` result.

Correctness counters are gates. Absolute latency and throughput are measurements, not CI pass/fail thresholds on shared runners.

## Checkin cost mix

Where instrumentation is available, publish a decomposition of one checkin:

```text
hash_us + encode_us + fsync_file_us + fsync_dir_us + sqlite_busy_us + sqlite_txn_us ~= wall_us
```

If the accounted components do not approximately explain wall time, treat the profile as incomplete before redesigning the storage or concurrency architecture.

## Raw results

A performance PR should attach or link the exact command and unedited machine-readable/raw output. A claim such as "10x faster" must state:

1. workload ID;
2. before and after commit SHAs;
3. concurrency point(s);
4. environment line;
5. correctness result;
6. p50/p95/p99 plus throughput;
7. the mechanism believed to explain the change.

## Interpretation rules

- Optimize a measured bottleneck, not an assumed one.
- Tail latency matters at least as much as mean throughput for many-agent workloads.
- Never weaken object or metadata durability to win a benchmark.
- Prefer the smallest change that produces a repeatable improvement.
- If a complex optimization does not materially improve the named workload without harming tails or correctness, remove it.
