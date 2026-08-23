# ForgeFS benchmark protocol

Performance claims must name a workload ID, hardware, durability policy, harness, and raw result source. A single ops/s number is not sufficient.

## Workloads

| ID | Workload | Independent variable | Correctness condition | Executable harness |
|---|---|---|---|---|
| W1 | Private agents, disjoint one-line files | agents: 1, 8, 32, 128, 512, 1k, 10k where practical | every checkin is `Updated`; final `fsck --full` is clean | `forge bench` / `private_checkins[_bounded]` |
| W2 | Shared-ref stampede | writers: 2, 8, 32, 128, 1k | exactly one `Updated`; all other writers `Forked`; `fsck --full` clean | `forge bench` / `shared_stampede[_bounded]` |
| W3 | Stale observation | two agents | checkin returns `StaleObservation`; destination ref does not advance | `e2e_stale_read_is_not_a_silent_success` |
| W4 | Same-path overlap | two agents | merge returns `MergeConflict`; conflict object preserves both immutable inputs | CLI/API merge-conflict e2e tests |
| W5 | Crash and reopen | one declared publish/checkpoint phase | after reopen, every committed ref resolves and `fsck --full` succeeds; otherwise the harness must exit non-zero with corruption | bootstrap/durability crash tests; real SIGKILL remains #147 |
| W6 | Large tree walk/update | entries: 10k, 100k, 1M | output tree is byte-for-byte identical; report lookup/update scaling | dedicated tree benchmark; do not infer W6 from W1/W2 |
| W7 | Git worktree comparison | W1 only | identical logical edit/checkin count and demonstrated durability-equivalence; otherwise mark `non-comparable` | checked-in comparator script required by #24 |

`forge bench` covers W1 and W2. W3-W5 are correctness/crash harnesses, not throughput substitutes. W6 is a scaling study. W7 is the external comparator.

## Fixed W1/W2 operation shape

Published W1/W2 results use the checked-in harness without local edits. The current harness defines one logical operation per agent:

- **W1 private:** grant one agent capability, open one session from `main`, write exactly one file `/w{i}.txt` containing UTF-8 `agent {i}`, then perform exactly one checkin. Timing for each sample starts before grant/session/write and ends after the checkin result; wall throughput covers the concurrent batch.
- **W2 shared:** create one `shared` ref, prepare one session and one unique `/s{i}.txt` write per writer, then start the timed region immediately before concurrent checkins. Each writer performs exactly one checkin against the same live ref. Preparation time is intentionally excluded; checkin contention is the measured variable.
- The serial section is a baseline, not a hidden warm-up. Do not discard it or silently add warm-up operations. If a platform requires cache warm-up for a separate experiment, report that as a distinct run class.
- Payload or tree-shape changes create a new benchmark variant and must not be compared under the same W1/W2 label without stating the variant.

Canonical command shape:

```bash
cargo run --release --locked -p forge-cli -- bench \
  --agents <N> --shared <M> --workers <W>
```

For a published performance claim, run at least **5 independent fresh-repository repetitions** at every reported concurrency point. Keep every repetition. Summaries report the median run-level throughput and median run-level p50/p95/p99/max; do not pool samples across runs unless the pooled distribution is also published separately.

## W3-W5 correctness status

Correctness workloads use exact typed/API outcomes, not prose:

- W3 passes only when the stale checkin returns `Error::StaleObservation` (CLI stable exit code `4` when exercised through the binary) and no ref publishes the stale write.
- W4 passes only when overlap returns `Error::MergeConflict` (CLI stable exit code `4`) and both sides remain reachable from the conflict object.
- W5 must name the fault mechanism and phase. A deterministic failpoint/bootstrap interruption and an OS `SIGKILL` are different evidence classes. Do not label a failpoint run as a physical power-loss or kill test. A committed dangling ref, silent repair, or successful exit after detected corruption is a failure.

The repository paths that define these semantics are `crates/forge-api/tests/e2e_concurrent.rs`, `crates/forge-cli/tests/e2e.rs`, `crates/forge-api/tests/bootstrap_contract.rs`, and `crates/forge-store/tests/meta_invariants.rs`. #147 is the gate for real process-kill evidence.

## Required environment line

Every published run records:

- ForgeFS commit SHA and build profile;
- exact command line and worker count;
- CPU model and logical-core count;
- RAM;
- OS and kernel/version;
- filesystem (for example ext4 or APFS);
- physical/storage device description when known;
- SQLite `journal_mode` and `synchronous` settings;
- whether macOS `fullfsync` is enabled;
- whether the run is cold, warm, or both;
- repetition number and fresh-repository path/class.

Do not compare results from different durability policies as if they were the same system.

### W7 comparator metadata and equivalence gate

A Git comparison additionally records:

- `git --version`;
- relevant `git config --show-origin --list` output for durability/performance-affecting settings;
- filesystem and storage settings for the Git worktree location;
- every explicit sync/fsync step in the comparator script;
- whether the compared operation includes object writes, ref update, index/worktree update, and any explicit durability barrier.

Before presenting a speed ratio, the report must explain why the Git path and ForgeFS path provide materially equivalent persistence for the measured operation. If equivalence cannot be demonstrated, publish both raw numbers but label the ratio **`non-comparable: durability mismatch/unknown`**. “Same durability intent” is not enough.

## Required metrics

For each concurrency point report at minimum:

- throughput (ops/s);
- latency p50, p95, p99, and max;
- successful `Updated`, `Forked`, `Noop`, stale, and conflict counts as applicable;
- SQLite busy/wait and transaction time;
- object puts and bytes;
- file-fsync and directory-fsync counts/time;
- CPU and peak RSS for long/large runs;
- final `fsck --full` result.

Correctness counters are gates. Absolute latency and throughput are measurements, not CI pass/fail thresholds on shared runners.

If the checked-in build cannot expose one of the required SQLite/fsync measurements, mark that field **`unavailable`** in the raw result. Such a run may support user-visible latency/throughput claims, but it is **not eligible for cross-version architectural attribution** (for example, “SQLite was the bottleneck” or “directory barriers caused the speedup”) until the missing instrumentation is available.

## Checkin cost mix

For architectural performance claims, publish the available decomposition of one checkin:

```text
hash_us + encode_us + fsync_file_us + fsync_dir_us + sqlite_busy_us + sqlite_txn_us ~= wall_us
```

Any unavailable component must be named. If the accounted components do not approximately explain wall time, treat the profile as incomplete before redesigning the storage or concurrency architecture.

## Raw results

A performance PR attaches or links the exact command and unedited machine-readable/raw output for **all repetitions**, not only the best run. A claim such as “10x faster” must state:

1. workload ID and any declared variant;
2. before and after commit SHAs;
3. concurrency point(s);
4. environment line;
5. correctness result;
6. all repetition outputs plus the median aggregation rule above;
7. p50/p95/p99/max plus throughput;
8. storage/SQLite instrumentation, or explicit `unavailable` markers;
9. the mechanism believed to explain the change;
10. for W7, the comparator metadata and durability-equivalence verdict.

## Interpretation rules

- Optimize a measured bottleneck, not an assumed one.
- Tail latency matters at least as much as mean throughput for many-agent workloads.
- Never weaken object or metadata durability to win a benchmark.
- Prefer the smallest change that produces a repeatable improvement.
- If a complex optimization does not materially improve the named workload without harming tails or correctness, remove it.
