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
| W8 | Read fanout: N readers re-reading a fixed path set through their own sessions | readers x reads | every read returns the seeded payload; catalog traffic lands mostly on the read pool | `forge bench --readers` / `read_fanout_bounded` |
| W7 | Git worktree comparison | W1 only | identical logical edit/checkin count and demonstrated durability-equivalence; otherwise mark `non-comparable` | `scripts/w7-git-comparator.sh` (#24) |

`forge bench` covers W1, W2 and (with `--readers`) W8. W3-W5 are correctness/crash harnesses, not throughput substitutes. W6 is a scaling study. W7 is the external comparator; run it with
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
  --agents <N> --shared <M> --readers <R> --reads <K> --workers <W>
```

`--readers` defaults to 0, so a bench invocation written before W8 existed runs
exactly the workloads it always ran. W8 seeds its own `readbench` ref with eight
paths and never touches `main`. `--reads` must be well above those eight paths
or the phase is another write workload wearing a different name: `Meta::observe`
looks a path up on a read connection and writes only when the row would change,
so the FIRST read of a path costs a write-mutex acquisition and every re-read of
it costs read-pool acquisitions alone.

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
- filesystem and storage settings for the **ForgeFS** location, and a
  statement that the two are the same filesystem. `forge bench` builds its
  workspace under `$TMPDIR` unless `--scratch` is given, which on many boxes
  is a different device from the repository; a ForgeFS row and a Git row taken
  on two filesystems are not a comparison. The comparator now passes
  `--scratch` under its own work directory so this cannot happen silently;
- the barrier-reach probe for that filesystem, as a published number and not
  as an assurance (see *Precondition: barriers must reach the device*);
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
ext2/ext3). ForgeFS commit `5a7017a`, release profile, `forge 0.3.0`, rustc
1.97.0. Git 2.39.5. SQLite `journal_mode=WAL`, `synchronous=FULL`, Linux
`fsync` path. Cold run class, fresh repository per repetition on both sides.

Precondition, published because the document demands it rather than because it
was convenient: every configuration ran under `/workspace/w7-final/work` on
`/dev/vdd` (ext4, `rw,relatime,discard`), and 200 `fsync`s on that filesystem
moved the device flush count by 200 -- **ratio 1.000, barriers reaching**. The
sandbox mounts `nobarrier` by default; `sudo mount -o remount,barrier
/workspace` cleared it first.

Workload W7 (W1 logical shape): 32 agents, 4 workers, 9 fresh-repository
repetitions per configuration. Medians are run-level medians (`median_low`,
so every published figure was observed in some repetition).

| Configuration | Durability | ops/s | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|---|
| ForgeFS, `forge bench` (in-process threads) | synchronous=FULL, object file+dir fsync | 245.8 | 15.08 | 19.12 | 20.61 | 20.61 |
| ForgeFS through the `forge` CLI (3 execs/agent) | synchronous=FULL, object file+dir fsync | 131.4 | 27.12 | 40.09 | 57.97 | 57.97 |
| Git worktrees (2 execs/agent) | git as shipped: no fsync config set | 344.9 | 10.65 | 15.79 | 18.10 | 18.10 |
| Git worktrees (2 execs/agent) | `core.fsync=all`, `core.fsyncMethod=fsync` | 254.7 | 14.06 | 20.21 | 23.34 | 23.34 |

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

Raw per-repetition JSON for all nine repetitions of all four configurations is
published under `docs/bench-results/w7-5a7017a/`, along with the barrier-reach
record and the environment line.

### Correction: the superseded `w7-63fa098` run

The earlier results in `docs/bench-results/w7-63fa098/` are **withdrawn as a
comparison**. Their per-repetition JSON is kept, because the raw output of a
run is not deleted for being wrong, but the table they supported should not be
quoted.

Two defects, in order of size:

1. **The ForgeFS and Git rows were on different filesystems.**
   `scripts/w7-git-comparator.sh` invoked `forge bench` without `--scratch`,
   so `forge bench` built its workspace under `$TMPDIR` -- on that box the
   container's overlay root -- while the `forge` CLI row and both Git rows ran
   under `--out .../work` on the ext4 volume. Only the in-process ForgeFS row
   was affected, and it was affected by a lot.

   Measured directly, one box, one binary, one commit, barriers reaching, 5
   repetitions per run and 3 runs of each variant:

   | row | `forge bench` on `$TMPDIR` (old) | all rows on one filesystem (fixed) |
   |---|---|---|
   | ForgeFS, `forge bench` | 405.7 / 342.5 / 364.8 | 179.8 / 195.6 / 211.1 |
   | ForgeFS, `forge` CLI | 115.9 / 118.4 / 120.8 | 129.2 / 124.9 / 126.8 |
   | Git worktrees, default | 251.4 / 262.7 / 272.1 | 270.8 / 306.3 / 303.0 |
   | Git worktrees, durable | 210.2 / 197.9 / 212.6 | 204.1 / 262.4 / 215.2 |

   The three rows that already shared a directory do not move. The one that
   did not, halves. In isolation the same `forge bench` invocation measured
   642 ops/s (median of 5) on the overlay against 197 ops/s on
   `/workspace` -- a 3.3x difference that has nothing to do with ForgeFS.

   The direction matters: the defect inflated the single row in which ForgeFS
   appeared to beat Git, and it did not touch the rows in which ForgeFS loses.
   A benchmark that can only err in its author's favour is the failure mode
   this document exists to prevent.

2. **Barrier reach was never recorded for that run.** The environment line has
   no field for it, and the W7 section did not state it. The old text read the
   near-free `core.fsync=all` (about 2%) as evidence that "this virtio device
   is not paying for a real cache flush". On the same device class with
   barriers proved reaching, `core.fsync=all` costs Git 26% (344.9 -> 254.7).
   The comparator now probes the flush count itself and prints the result
   above the table, and labels the table plainly when barriers do not reach.

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
  how an agent orchestrator drives either tool -- **ForgeFS loses**: 131.4
  ops/s against 344.9 (git default) and 254.7 (git durable). ForgeFS is
  doing strictly more durability work in that row, but a slower row is a
  slower row.
- **ForgeFS also loses the in-process row**, once that row is measured on the
  same filesystem as everything else: 245.8 against 344.9. It edges ahead of
  git at `core.fsync=all` (245.8 against 254.7 -- inside the run-to-run spread,
  so call it level), and even that comparison hands ForgeFS the process model:
  `forge bench` uses in-process threads while Git execs twice per agent. There
  is currently no configuration of this workload on this box in which ForgeFS
  is faster than Git.
- The no-op Git exec floor on this box measured 1176.1 ops/s (p50 2.99 ms)
  under the same driver. Subtract it before attributing any part of the CLI
  row's gap to storage design.
- `core.fsync=all` costs Git 26% here. Do not carry these numbers to hardware
  where `fsync` is expensive: there ForgeFS issues far more barriers per
  operation than Git and would be expected to lose by more, not less. Re-run
  the comparator on the target device.
- Worktree and branch creation is excluded from the Git timed region (median
  0.562 s for 32 worktrees, measured and reported separately), while ForgeFS
  capability grant and session open are inside the W1 timed region. That
  exclusion favours Git.

Correctness gates passed in every repetition: `fsck --full` inside `forge
bench`, `forge fsck --full` for the CLI configuration, and `git fsck --strict`
plus a per-agent check that each branch carries exactly one new commit with
the exact bytes. Raw per-repetition JSON for all nine repetitions of all four
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
- `write_acquires` / `write_wait_us` and `read_acquires` / `read_wait_us`
  on the `sqlite locks` line decompose that sum: the write connection's mutex,
  which every catalog write and every read that fell back to the writer queues
  on, against the read pool's slot mutexes. `write_share_of_wait` is
  `write_wait_us / lock_wait_us`, and it is `n/a` rather than a number when
  nothing waited at all. **Writer contention is read here and nowhere else.**
  Until issue #324 the renderer printed only the sum, so a writer convoy and a
  busy read pool produced the same line; a barriered-storage sweep across four
  storage classes had to report writer contention as `unavailable` for exactly
  that reason, and `lock_wait_us` alone had by then produced three wrong
  conclusions on #37. `forge stats --json` exposes the same split under
  `write_lock_acquires` / `write_lock_wait_us` and `read_lock_acquires` /
  `read_lock_wait_us`.

  The two halves separate workloads the sum cannot. One pair of runs on the same
  machine, `--workers 16`:

  | Run | `lock_wait_us` (the sum) | `write_wait_us` | `read_wait_us` | `write_share_of_wait` |
  |---|---:|---:|---:|---:|
  | `--agents 192 --shared 96` (W1+W2) | 1704942 | 1653635 | 51307 | 97.0% |
  | `--readers 10 --reads 1000` (W8) | 1701393 | 339144 | 1362249 | 19.9% |

  The summed column is the same number to any reader. The split says one run is
  a writer convoy and the other is not. Amounts are environment-dependent and
  are quoted here to show the SHAPE of the difference, not as a result.
- `txn_count` counts every write transaction SQLite committed on the catalog:
  each explicit `BEGIN IMMEDIATE` that committed, and each autocommit statement
  that wrote, since SQLite wraps every such statement in an implicit
  transaction. It is taken from SQLite's commit hook, so no write path can be
  missing from it, and a rolled-back or read-only transaction is absent from it
  because nothing was committed. Schema setup during `Meta::open` is excluded.
  It is a transaction count and never a row count -- `Meta::row_mutations`
  (`sqlite3_total_changes`) is the instrument for write amplification.
- `explicit_txn_count` / `txn_us` cover every instrumented explicit
  `BEGIN IMMEDIATE` attempt from before BEGIN through COMMIT or rollback.
  SQLite's cross-process `busy_timeout` wait is therefore inside `txn_us`;
  `busy` is an outcome count, not a duration. Schema setup during `Meta::open`
  and implicit autocommit statements are not included in `txn_us`, so
  `explicit_txn_count` -- not `txn_count` -- is the sample count that pairs
  with it.

  Before issue #311 the field named `txn_count` held what is now
  `explicit_txn_count`, and a phase whose only catalog writes were autocommit
  reported zero transactions. `forge stats --json` bumped `schema_version` to 2
  to mark the boundary.
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

## Precondition: barriers must reach the device

A durability measurement taken on a filesystem that discards write barriers is
not a measurement, and no amount of repetition fixes it. Establish barrier reach
*before* the first run and publish the check with the numbers:

```sh
grep -w /workspace /proc/mounts        # must not contain `nobarrier`
```

Then confirm empirically that an `fsync` becomes a device flush. Field 19 of the
matching `/proc/diskstats` row is "flush requests completed"; N fsyncs to a file
on that filesystem must move it by about N:

```sh
awk '$3=="vdd"{print $19}' /proc/diskstats   # before
# ... N fsyncs ...
awk '$3=="vdd"{print $19}' /proc/diskstats   # after
```

A ratio near zero means the barriers stop before the platter. Report the
environment as barrier-less, do not publish durability numbers from it, and move
to a path where the ratio is near one. `nobarrier` can sometimes be cleared with
`mount -o remount,barrier <mountpoint>`; verify with the flush count afterwards
rather than trusting the mount line alone.

Report the device-flush delta alongside throughput at every concurrency point.
That is what separates "we issue fewer barriers" and "our barriers overlap"
from "we got a faster number", and it is the only way a durability-preserving
speedup can be told apart from a durability-weakening one.

## Group commit on the metadata catalog

Every catalog write is `BEGIN IMMEDIATE ... COMMIT` under `synchronous=FULL`,
so SQLite's WAL fsync happens inside `COMMIT`, and `COMMIT` runs while the
process-wide write mutex is held. The mutex, not the fsync, was the ceiling:
concurrent fsyncs are cheap because the kernel already group-commits them, but
a serialised critical section never lets two of them overlap.

`Meta::run_grouped` queues a write and then competes for the write connection
as before. The winner drains the queue and runs every waiting job in one
`BEGIN IMMEDIATE ... COMMIT`, each inside its own `SAVEPOINT` so one caller's
rejection is not the batch's. N waiting writers therefore pay one WAL fsync
instead of N, and the per-write barrier cost falls as concurrency rises instead
of staying flat.

Durability is unchanged, and the reason is structural rather than statistical:
`synchronous=FULL` is untouched, and a waiter is told it succeeded only after
the leader's `COMMIT` -- the same fsyncing commit -- has returned. A waiter
knows its own transaction was in that fsync because the leader owns both the
job and the reply channel: it executes the job in the transaction it is about
to commit and sends on that channel only afterwards. Membership is never
inferred from a sequence number, a timestamp, or a batching window. There is no
window: a job not yet executed is still in the queue with its caller still
blocked, so no acknowledgement can outrun its own barrier.

With a single writer a batch holds one job and the emitted SQL is what it
always was, which is why `txn_count` is unchanged at W=1 and why the
uncontended latency is unchanged too. `txn_count` comes from SQLite's commit
hook, so the ratio of catalog writes to `txn_count` is the achieved batch depth,
and it is the counter to quote when attributing a change to this mechanism.

## Directory barriers: the measured budget, and what collapsing it buys

### The budget, verified before it was attacked

Barrier reach was established first, by the procedure under *Precondition:
barriers must reach the device* above. Every ext4 mount in the box shipped
`nobarrier`; `sudo mount -o remount,barrier /workspace` cleared it and 200
`fsync`s then moved field 19 of the matching `/proc/diskstats` row by 200 —
**ratio 1.000**, re-checked before every campaign. `discard` was also cleared (`remount,nodiscard`) because TRIM storms
from repeated fresh-repository setup were the largest source of run-to-run
drift; that option affects neither barriers nor durability.

`crates/forge-api/tests/dir_barrier_budget.rs` is the instrument. Unlike the
process-lifetime counters it takes its deltas around the timed region only, so
`Forge::init`, capability setup and the final `fsck` are outside the window and
the per-checkin figures below are measurements, not divisions of a lifetime
total. It reports the device flush delta beside them.

For the W1 shape (one blob write plus one checkin, four new objects) the
per-checkin budget on `main` reproduced exactly:

```
flushes/checkin = 4 file + (2 x new_objects + P(new first-level shard)) dir + 3 sqlite
                = 4 + (2 x 4 + 1.01) + 3
                = 16.01 issued,  16.07 completed by the device at W=1
```

The nine directory barriers are two per new object — `fsync` on the parent
that just gained a shard entry, and `fsync` on the leaf that just gained the
object entry — plus about one `objects/` barrier per checkin while first-level
shards are still new. Content addressing scatters four objects across four
distinct leaf directories, so within-batch deduplication almost never fires.

### Three policies, same durable state

`DirectoryBarrier` selects how a `PublishBatch` proves its edges.
`FORGEFS_DIR_BARRIER=per-directory|deferred|collapsed` overrides the default,
which is `per-directory`.

| Policy | Barriers | Portable |
|---|---|---|
| `per-directory` (default) | one `fsync` per touched directory, taken as the batch touches it (the shape before #177) | yes |
| `deferred` | one `fsync` per *distinct* touched directory, all of them in a single phase immediately before `finish` returns | yes |
| `collapsed` | one `syncfs(2)` for the whole batch, shared with any concurrent batch also waiting for one | Linux only |

All three publish the same durable state, and the argument is an ordering one.
When `finish` returns, every object file the batch published or joined, every
shard entry it created, and every leaf entry naming one of its objects is
durable; the ref CAS runs strictly after that. Order *within* the barrier
phase is unconstrained because nothing between `put_parts` and `finish` is
observable: a crash at any point before the CAS leaves durable orphan objects
and possibly half-proved shard directories that no ref names, which is what
`fsck --full` walking from refs calls clean, and which a later batch re-proves
from cold caches rather than trusting. `collapsed` is *stronger* than the set
it replaces, not weaker: `syncfs` forces every dirty inode and page on the
filesystem, a strict superset of this batch's bytes and edges.

The load-bearing detail in `deferred` and `collapsed` is that the positive
proof — the process-wide "this directory entry is durable" cache that lets a
later batch skip a barrier — is published in `finish` and nowhere else, exactly
as OID proofs already were. Recording it when the directory is *created*
instead would let the next batch skip a barrier for an edge that was never
durable, and then CAS a ref onto an object a crash can still lose.
`deferred_unfinished_batch_publishes_no_directory_proof` and its collapsed twin
are that regression.

For `collapsed`, membership in a shared barrier is never inferred from a window
or a timestamp. A batch reads the gate's generation counter after its own
`link(2)` calls have returned and waits for the first generation that cannot
have started before that read; `completed` advances by exactly one per
successful barrier, so being released proves that generation ran to completion
after this batch's work was already in the kernel. A failed barrier advances
nothing, so no waiter inherits a proof that does not exist.

### Measured

Box: islo sandbox, AMD EPYC 9454, 4 logical cores, 7.8 GiB RAM, Debian 12,
kernel 6.16.9, virtio block device `/dev/vdd` at `/workspace`, ext4
`rw,relatime` (barriers on, verified 1.000). ForgeFS `c81f63d` plus this
change, release profile, rustc 1.98.0, `forge 0.2.1`. SQLite `journal_mode=WAL`,
`synchronous=FULL`, Linux `fsync` path. Cold run class, fresh repository per
repetition. 240 checkins per run, 5 repetitions per point, policies interleaved
back-to-back at each point so device drift cancels. Medians are run-level
`median_low`. `fsck --full` clean in every run.

```bash
FORGEFS_DIR_BARRIER=<policy> FORGEFS_BB_DEV=vdd FORGEFS_BB_N=240 FORGEFS_BB_W=<W> \
  FORGEFS_BB_DIR=<fresh path> \
  cargo test --release --locked -p forge-api --test dir_barrier_budget -- --ignored --nocapture
```

| W | policy | ops/s | p50 ms | p99 ms | dir barriers/checkin | issued flushes/checkin | device flushes/checkin |
|---:|---|---:|---:|---:|---:|---:|---:|
| 1 | per-directory | 154.7 | 6.33 | 10.44 | 9.00 | 16.00 | 16.07 |
| 1 | deferred | 165.7 | 5.87 | 9.12 | 8.68 | 15.68 | 15.75 |
| 1 | collapsed | 163.3 | 5.95 | 11.10 | 2.00 | 9.00 | 9.07 |
| 2 | per-directory | 291.7 | 6.27 | 14.92 | 9.02 | 16.02 | 16.09 |
| 2 | deferred | 292.2 | 6.42 | 13.39 | 8.70 | 15.70 | 15.77 |
| 2 | collapsed | 248.5 | 7.29 | 13.34 | 2.00 | 9.00 | 9.07 |
| 4 | per-directory | 439.2 | 8.48 | 15.60 | 9.01 | 16.01 | 13.30 |
| 4 | deferred | 449.3 | 8.56 | 13.22 | 8.73 | 15.73 | 13.28 |
| 4 | collapsed | 354.9 | 10.71 | 17.55 | 1.83 | 8.83 | 8.11 |
| 8 | per-directory | 516.7 | 14.13 | 28.97 | 9.01 | 16.01 | 10.53 |
| 8 | deferred | 513.9 | 14.37 | 29.87 | 8.72 | 15.72 | 11.00 |
| 8 | collapsed | 401.9 | 18.08 | 33.53 | 1.58 | 8.59 | 7.19 |
| 16 | per-directory | 503.9 | 29.36 | 51.16 | 9.01 | 16.01 | 10.71 |
| 16 | deferred | 546.2 | 28.11 | 45.75 | 8.72 | 15.72 | 10.73 |
| 16 | collapsed | 411.9 | 36.10 | 61.91 | 1.51 | 8.52 | 7.09 |

The cost model matches the counters at every point. `per-directory` and
`deferred` issue `4 + dir + 3`; `collapsed` issues `4 + barrier_fs + 3`, where
`barrier_fs` falls from 2.00 per checkin at W=1 to 1.51 at W=16 as concurrent
batches share barriers (`barrier_fs_batches / barrier_fs` is the achieved
sharing depth). At W=1 the device completes what was issued to within 0.5%.

### What the numbers say, without decoration

**The nine directory barriers collapse to two, and that is not a speedup.**
`collapsed` removes 7.0 of 16.1 device flushes per checkin — a 1.77x reduction
in barrier count with I4 unchanged — and buys **+5.6% at W=1 and loses 15% to
22% at every higher concurrency**. It is not the default and this table is why.

Two measurements explain it, and both refute the linear model that made nine
directory barriers look like nine sixteenths of the cost:

- **A flush's marginal cost is not its average cost.** At W=1, `collapsed`
  eliminates 7.0 device flushes per checkin and saves 0.34 ms of a 6.46 ms
  checkin: **49 us per eliminated directory flush**, against an average of
  402 us across all sixteen. A directory `fsync` that follows other barriers
  finds the journal already committed and costs a bare device flush, which
  this virtio device answers in tens of microseconds. Dividing a checkin's
  wall time by its flush count assigns that cost to the wrong barriers.
- **The kernel already amortises them.** At W=16 `per-directory` issues 16.01
  barriers per checkin and the device completes 10.71: jbd2 merges concurrent
  fsyncs into shared journal commits, recovering a third of the budget for
  free. `syncfs`, by contrast, is a whole-filesystem writeback and a global
  serialisation point — one at a time, each paying for every dirty inode on
  the filesystem, including the peers' — so trading ten cheap merged barriers
  for seven expensive serialised ones loses.

`per-directory` stays the default, and the same table is why. `deferred` is
the tidier shape — it proves the same edges with the same primitive,
deduplicates them, and takes them all in one phase immediately before the ref
CAS — but it measures *within noise* of `per-directory` at every point (+7.1%
at W=1, +8.4% at W=16, -0.5% at W=8), so no throughput claim is made for it.
The rule in *Interpretation rules* below applies to a default as much as to a
new knob: a change that does not materially improve is not worth its risk, and
this one lands in the durability-critical path, where the shape that has been
exercised the longest is worth more than a delta the instrument cannot
distinguish from drift. `deferred` remains selectable, and is the setting to
reach for on a filesystem whose journal commit dominates its device flush.

`collapsed` remains available and is the right setting where a barrier is
genuinely expensive relative to filesystem writeback — a device whose flush
costs milliseconds rather than the ~49 us marginal cost measured here, or a
repository on a filesystem it does not share with a busy writer. That crossover
was not measured; this box cannot produce it, and no claim is made about it.

**Do not use `collapsed` on a filesystem shared with an unrelated heavy
writer.** Every checkin then waits for that writer's dirty data.

### Crash evidence and its exact boundary

`scripts/dir-barrier-sigkill.sh` is the harness: `kill -9` under sustained
concurrent load, eight writer loops of `session open` / `write` / `checkin` as
separate processes, each appending `updated <ref> <oid>` to a tmpfs log **only
after** its checkin process exited 0 with that line. The log is kernel memory,
so a SIGKILL of the writers cannot lose an acknowledgement the repository must
then account for. The repository is then reopened cold, `fsck --full` must
pass, and every acknowledged ref must resolve to exactly its acknowledged oid.

| policy | runs | acknowledged checkins | fsck | lost |
|---|---:|---:|---|---:|
| deferred | 6 | 3223 | clean | 0 |
| collapsed | 4 | 2308 | clean | 0 |

**This proves process-crash durability and not power loss.** SIGKILL leaves the
page cache intact, so every byte the killed processes wrote is still in memory
and reaches the platter afterwards. The proof is worth stating precisely
because it is easy to overclaim: the same harness was run against a build
carrying a deliberate I4 violation — the deferred directory proof recorded when
the directory is created rather than after its barrier — and it passed
**1960 acknowledged checkins over 3 runs with a clean `fsck --full` and zero
losses**. A process-kill test cannot see this class of defect. What catches it
is `deferred_unfinished_batch_publishes_no_directory_proof`, which fails on
that build with

```
assertion `left == right` failed: objects->aa, aa->bb and bb->OID must all be
proved by the batch that is about to let a ref name the object
  left: 1
 right: 3
```

The power-loss half of the argument therefore rests entirely on the device
flush counts in the table above and on the verified 1.000 barrier-reach ratio.
Closing the gap needs a device whose volatile cache can be discarded —
`dm-log-writes` replay or `dm-flakey drop_writes`. Neither is available in this
sandbox: it ships no `dmsetup`, no `losetup`, and no loadable kernel modules.
That evidence is owed, and is not claimed here.

## Interpretation rules

- Optimize a measured bottleneck, not an assumed one.
- Tail latency matters at least as much as mean throughput for many-agent workloads.
- Never weaken object or metadata durability to win a benchmark.
- Prefer the smallest change that produces a repeatable improvement.
- If a complex optimization does not materially improve the named workload without harming tails or correctness, remove it.
