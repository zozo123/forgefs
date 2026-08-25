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
| W7 | Git worktree comparison | W1 only | identical logical edit/checkin count and demonstrated durability-equivalence; otherwise mark `non-comparable` | `scripts/w7-git-comparator.sh` (#24) |

`forge bench` covers W1 and W2. W3-W5 are correctness/crash harnesses, not throughput substitutes. W6 is a scaling study. W7 is the external comparator; run it with
`scripts/w7-git-comparator.sh` and read its verdict before quoting any
ForgeFS/Git number.

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

By default the CLI creates and owns a fresh temporary workspace and removes it
after a successful run. To retain a run for inspection or crash testing, pass
`--scratch <new-path>`; the path must not already exist. The global
`--dir`/`FORGE_DIR` repository selector is deliberately rejected by `bench` and
is never interpreted as disposable storage.

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

## W7 results: ForgeFS against Git worktrees

The comparator is checked in as `scripts/w7-git-comparator.sh` (Git side and
gate rules in `scripts/w7_git_worktree_bench.py`, optional durability-barrier
probe in `scripts/w7_fsync_probe.c`):

```bash
cargo build --release --locked -p forge-cli
scripts/w7-git-comparator.sh --agents 32 --workers 4 --reps 5 --out results/w7
```

It runs one logical workload -- N agents, one small edit each, one commit each
onto that agent own ref -- in four configurations, keeps every repetition, and
refuses to print a speed ratio unless it has observed both paths issuing the
same classes of durability barrier.

The four configurations exist because two of them can lose. `forge bench`
drives agents as in-process threads; Git must exec twice per agent. The
`forge` CLI configuration runs identical ForgeFS work with three execs per
agent, which is what an orchestrator that shells out actually pays.

### Measured run

Box: islo sandbox, AMD EPYC 9454, 4 logical cores, 7.8 GiB RAM, Debian 12,
kernel 6.16.9, virtio block device `/dev/vdd` mounted at `/workspace` (`df`
reports ext4; the environment-line probe renders the `stat -f` magic as
ext2/ext3). ForgeFS commit `63fa098`, release profile, `forge 0.1.0`, rustc
1.97.0. Git 2.39.5. SQLite `journal_mode=WAL`, `synchronous=FULL`, Linux
`fsync` path. Cold run class, fresh repository per repetition on both sides.

Workload W7 (W1 logical shape): 32 agents, 4 workers, 5 fresh-repository
repetitions per configuration. Medians are run-level medians (`median_low`,
so every published figure was observed in some repetition).

| Configuration | Durability | ops/s | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|---|
| ForgeFS, `forge bench` (in-process threads) | synchronous=FULL, object file+dir fsync | 462.9 | 8.39 | 10.17 | 12.86 | 12.86 |
| ForgeFS through the `forge` CLI (3 execs/agent) | synchronous=FULL, object file+dir fsync | 198.3 | 19.00 | 28.51 | 39.71 | 39.71 |
| Git worktrees (2 execs/agent) | git as shipped: no fsync config set | 289.2 | 14.11 | 19.51 | 22.24 | 22.24 |
| Git worktrees (2 execs/agent) | `core.fsync=all`, `core.fsyncMethod=fsync` | 283.3 | 12.65 | 20.44 | 25.14 | 25.14 |

Durability barriers observed for one agent operation, counted by
`LD_PRELOAD`-interposing `fsync`/`fdatasync` and classifying each descriptor
with `fstat`:

| Path | file barriers | directory barriers |
|---|---|---|
| ForgeFS `write` + `checkin` | 6 | 20 |
| Git `add` + `commit`, as shipped | 0 | 0 |
| Git `add` + `commit`, `core.fsync=all` | 6 | 0 |

The ForgeFS census spans two CLI processes, each of which performs its own
open-time object/tmp directory durability setup, so 6/20 is an upper bound on
one in-process checkin. The gate reads only presence or absence of a barrier
class, which that overcount cannot flip.

### Equivalence verdict

- Git as shipped: **`non-comparable: durability mismatch`**. Git issued zero
  durability barriers for the measured operation while ForgeFS issued both
  file and directory barriers. The two ops/s numbers stand; their quotient is
  not a speed ratio.
- Git with `core.fsync=all`: **`non-comparable: durability mismatch`**. Git
  fsyncs the object files but never their containing directories, so a crash
  can still lose a directory entry ForgeFS has already made durable. Closer,
  still not equivalent, still not a ratio.

Git 2.39.5 has no configuration that adds the directory barrier, so on this
platform W7 cannot currently produce a comparable ratio at all. That is the
honest state of the comparison, not a placeholder.

### What the numbers say, without decoration

- Compared like for like -- one process invocation per agent step, which is
  how an agent orchestrator drives either tool -- **ForgeFS loses**: 198.3
  ops/s against 289.2 (git default) and 283.3 (git durable). ForgeFS is
  doing strictly more durability work in that row, but a slower row is a
  slower row.
- ForgeFS wins only in the in-process `forge bench` row, and that row is not
  comparable to Git on process model. The no-op Git exec floor on this box
  measured 1639.2 ops/s (p50 2.20 ms) under the same driver.
- `core.fsync=all` cost Git about 2% here. A barrier that nearly free means
  this virtio device is not paying for a real cache flush. Do not carry these
  numbers to hardware where `fsync` is expensive: there ForgeFS issues far
  more barriers per operation than Git and would be expected to lose by more,
  not less. Re-run the comparator on the target device.
- Worktree and branch creation is excluded from the Git timed region (median
  0.807 s for 32 worktrees, measured and reported separately), while ForgeFS
  capability grant and session open are inside the W1 timed region. That
  exclusion favours Git.

Correctness gates passed in every repetition: `fsck --full` inside `forge
bench`, `forge fsck --full` for the CLI configuration, and `git fsck --strict`
plus a per-agent check that each branch carries exactly one new commit with
the exact bytes. Raw per-repetition JSON for all five repetitions of all four
configurations is what the script writes to its `--out` directory; publish it
with any claim made from this section.

### Not measured here: what a worktree does not provide

This section contains no measurements, and none of it justifies a throughput
claim. Throughput is not the axis on which the two differ:

- **Snapshot-pinned reads.** A ForgeFS session reads and checks in from one
  pinned commit (I8). A worktree reads whatever the working tree currently
  holds, and a concurrent write is visible mid-operation.
- **Stale-observation detection.** A checkin from a stale pin fails with
  `StaleObservation` (W3, CLI exit code 4). Git has no equivalent: a rebase or
  reset can silently move the ground under a long-running agent.
- **Fork on lose, never a hidden retry.** A losing CAS publishes a fork rather
  than overwriting or retrying (I18: a refused checkin never destroys staged
  work). The Git analogue is a failed push or a force-push, and the recovery
  is manual.
- **Capability-scoped authority.** Each agent operates under an attenuated
  macaroon bound to specific refs and operations (I13/I14). A worktree grants
  whatever the filesystem grants.
- **Provenance and sealed release.** Contribution and Conflict objects,
  deterministic merge, and ed25519-sealed tags verified from durable bytes
  (I11/I12/I15). Git offers signed tags but no typed contribution graph.

These are the reasons to choose ForgeFS. The measured numbers above are not.

## Required metrics

For each concurrency point report at minimum:

- throughput (ops/s);
- latency p50, p95, p99, and max;
- successful `Updated`, `Forked`, `Noop`, stale, and conflict counts as applicable;
- whole-run process-lifetime SQLite mutex acquisitions/wait, explicit transaction attempts/time, and busy outcomes;
- object puts and bytes;
- whole-run process-lifetime file-fsync and directory-fsync counts/time;
- CPU and peak RSS for long/large runs;
- final `fsck --full` result.

Correctness counters are gates. Absolute latency and throughput are measurements, not CI pass/fail thresholds on shared runners.

If the checked-in build cannot expose a required measurement, render that
specific field as **`unavailable`** in the raw result. Such a run may support
user-visible latency/throughput claims, but it is **not eligible for
architectural attribution involving the missing field** until instrumentation
is available. For example, do not claim byte amplification, SQLite contention,
or directory-barrier improvements when the corresponding field is unavailable.

## Whole-run process-lifetime counters

The raw counter block emitted by `forge bench` is one cumulative lifetime
snapshot, not a workload delta and not a checkin profile. Its boundaries are:

1. Storage counting starts inside `LocalBlobStore::new` for the `Store` retained
   by the returned `Forge`, before that open's counted object/tmp directory
   durability setup and stale-temp cleanup. During `Forge::init`, this is the
   post-publication reopen; counters owned by the discarded staging `Store` are
   not part of the report.
2. SQLite counting starts after `Meta::open` finishes for that retained Store;
   schema/open work is excluded, while the following seal-key validation read
   is included. API outcome counting starts when the final `Forge` is built.
3. Counting continues across the serial baseline, private workload, shared
   workload, merge/seal, and tag verification.
4. The snapshot is taken after verification. The bounded-worker runner also
   performs and includes full `fsck` before taking the snapshot.

The individual counter semantics are deliberately mechanical:

- `puts` counts newly published OIDs. Object-byte accumulation is not yet
  instrumented, so the renderer emits the explicit literal
  `bytes=unavailable`; do not derive bytes from puts.
- `fsync_file` / `fsync_file_us` count and time successful file durability
  barriers; `fsync_dir` / `fsync_dir_us` do the same for directories. Failed
  barriers fail the operation and are not reported as successful work.
- `lock_acquires` / `lock_wait_us` cover every acquisition of a process-local
  SQLite connection mutex, including reads and autocommit writes. That is the
  write connection plus the read-only connections that serve SELECT-only
  catalog queries, so the pair is a sum across connections and not a measure of
  writer contention on its own.
- `txn_count` / `txn_us` cover every instrumented explicit `BEGIN IMMEDIATE`
  attempt from before BEGIN through COMMIT or rollback. SQLite's
  cross-process `busy_timeout` wait is therefore inside `txn_us`; `busy` is an
  outcome count, not a duration. Schema setup during `Meta::open` and implicit
  autocommit statements are not included in `txn_us`.
- Rendered `lifetime_accounted_us = lock_wait_us + txn_us` and
  `lifetime_barrier_us = fsync_file_us + fsync_dir_us` are saturating
  arithmetic sums over the lifetime snapshot, not per-operation measurements.

Durations are accumulated internally in nanoseconds and converted to whole
microseconds only when read, so a sequence of sub-microsecond lock waits is not
silently rounded to zero. Cumulative phase durations from concurrently
executing operations can overlap in wall-clock time.

The renderer prints their arithmetic aggregation only as a lifetime phase
total:

```text
fsync_file_us + fsync_dir_us + sqlite_lock_wait_us + sqlite_txn_us = cumulative_phase_us
```

These totals may support whole-run regression diagnosis when command shape and
boundaries are identical. They **must not** be divided by a checkin count,
compared with one checkin's latency, or described as an average/p50/p99
checkin cost. They include different phase populations, and concurrent phase
durations can overlap.

## Machine-readable counters

`forge stats --json` emits the same process-lifetime counters as one stable
JSON document; `CLI_ABI.md` owns its key set. It carries the identical scope
boundary: those totals are cumulative for one process and never a per-checkin
measurement. It is a counter surface, not a benchmark protocol -- a claim
about ForgeFS performance still comes from this document, not from that one.

## Per-checkin cost mix: unavailable

A true checkin mix remains follow-up instrumentation. It requires
operation-scoped counter snapshots or tracing that begins and ends with the
same checkin and excludes initialization, other workloads, merge/seal,
verification, and `fsck`. Hashing, canonical encoding, and SQLite autocommit
work also remain uninstrumented.

Until that attribution exists, every run reports the per-checkin mix as
`unavailable`. Do not estimate it by dividing the process-lifetime totals. The
target future decomposition remains:

```text
hash_us + encode_us + fsync_file_us + fsync_dir_us + sqlite_wait_us + sqlite_txn_us ~= checkin_wall_us
```

Any unavailable component must be named. Do not redesign the storage or
concurrency architecture based on an incomplete attribution.

## Raw results

A performance PR attaches or links the exact command and unedited machine-readable/raw output for **all repetitions**, not only the best run. A claim such as “10x faster” must state:

1. workload ID and any declared variant;
2. before and after commit SHAs;
3. concurrency point(s);
4. environment line;
5. correctness result;
6. all repetition outputs plus the median aggregation rule above;
7. p50/p95/p99/max plus throughput;
8. whole-run storage/SQLite lifetime totals, including `bytes=unavailable`,
   plus explicit `unavailable` markers for per-checkin attribution;
9. the mechanism believed to explain the change;
10. for W7, the comparator metadata and durability-equivalence verdict.

## Interpretation rules

- Optimize a measured bottleneck, not an assumed one.
- Tail latency matters at least as much as mean throughput for many-agent workloads.
- Never weaken object or metadata durability to win a benchmark.
- Prefer the smallest change that produces a repeatable improvement.
- If a complex optimization does not materially improve the named workload without harming tails or correctness, remove it.
