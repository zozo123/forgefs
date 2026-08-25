# ForgeFS

A **concurrency, isolation, and provenance substrate for autonomous agents**.

ForgeFS is not another POSIX filesystem and not S3 copy-in/copy-out. Bytes are immutable and content-addressed. Agents reason from pinned snapshots. Named refs publish by compare-and-swap. Checkin detects both **write conflicts** and **stale observations**. Losing races fork instead of clobbering work. Releases are cryptographically sealed and verified from durable bytes.

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

## Install

```bash
# Released binary (linux x86_64; four targets are published)
V=0.2.1; T=x86_64-unknown-linux-gnu
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/forge-$V-$T.tar.gz
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS      # verify before you run it
tar -xzf forge-$V-$T.tar.gz && sudo install forge-$V-$T/forge /usr/local/bin/
forge --version
```

Every release asset is covered by `SHA256SUMS` — the binaries **and** the gate evidence beside them —
and each build carries a SLSA provenance attestation you can check without trusting this page:

```bash
gh attestation verify forge-$V-$T.tar.gz -R zozo123/forgefs
```

From source instead: `cargo install --locked --git https://github.com/zozo123/forgefs forge-cli`,
or clone and use `cargo run -p forge-cli --` in place of `forge` below.

## 60-second two-agent path

```bash
forge init ./demo

ROOT=./demo/.forge/keys/root.cap
INT=./demo/.forge/keys/integrator.cap

ALICE=$(forge --dir ./demo --cap $ROOT grant \
  --ops read,write,branch --ref 'main,heads/agents/alice/*' --agent alice)
BOB=$(forge --dir ./demo --cap $ROOT grant \
  --ops read,write,branch --ref 'main,heads/agents/bob/*' --agent bob)

A=$(forge --dir ./demo --cap "$ALICE" session open --from=main)
B=$(forge --dir ./demo --cap "$BOB" session open --from=main)

forge --dir ./demo --cap "$ALICE" write --ns "$A" /a.txt --text alice
forge --dir ./demo --cap "$BOB"   write --ns "$B" /b.txt --text bob
forge --dir ./demo --cap "$ALICE" checkin --ns "$A" -m alice
forge --dir ./demo --cap "$BOB"   checkin --ns "$B" -m bob

forge --dir ./demo --cap $INT merge --into=main --from="heads/agents/alice/$A"
forge --dir ./demo --cap $INT merge --into=main --from="heads/agents/bob/$B"
forge --dir ./demo --cap $INT seal main --tag v1.0 --attest
forge --dir ./demo --cap $ROOT verify v1.0
forge --dir ./demo --cap $ROOT fsck --full
```

The last four lines print, on a clean run:

```text
sealed tags/v1.0 <oid>
attested ok
ok <oid>
ok (full): 4 refs, 15 objects, 2 namespaces
```

Two agents, one forge, no shared mutable checkout. Same-path edits become a **Conflict object**. If one agent changes `/x` after another observed its old OID, the stale agent's later checkin fails even when it only writes `/y`. If writers race one mutable ref, one wins and every loser becomes an explicit fork — and that fork carries the loser's completed contribution, so a refused checkin never throws staged work away (I18).

## The contract, and who is on the hook for it

[INVARIANTS.md](INVARIANTS.md) is eighteen numbered rules, I1 through I18, and it is the file to read
second. It is not a manifesto: it ends with a **traceability table** that maps every invariant to the
production module that owns it and to the exact test files that prove it — property tests that state
an invariant as algebra and drive it from a seeded generator, fuzz targets that feed the same
boundaries untrusted bytes, and real race, process, crash and filesystem harnesses kept deliberately
out of the shared table test so that a real boundary is not diluted into a mock. If you want to know
whether a claim on this page is load-bearing, that table tells you where to look.

*A PR that cannot name an invariant does not merge.* See [AGENTS.md](AGENTS.md) for the contributor
architecture and change rules, and [FORMAT.md](FORMAT.md) for the v1 object encoding, which is frozen.

## Integrity is a product surface

```bash
# refs, seals, namespaces, mounts, observations, and reachable typed object closure
forge --dir ./demo --cap $ROOT fsck

# additionally prove SQLite structure/catalog relations and scan every object file
forge --dir ./demo --cap $ROOT fsck --full

# machine-readable report for CI/agents
forge --dir ./demo --cap $ROOT fsck --full --json
```

`fsck` is read-only. It bypasses hot object caches, rehashes durable bytes, verifies expected type on every graph edge, checks sealed tags against the trusted local seal key, validates namespace/live-ref/mount state, and bounds graph work. `fsck --full` also runs SQLite integrity checking from one coherent read transaction; proves the schema ledger, ref/reflog terminal state and chain, seal/tag/landmark closure, provenance rows, and namespace-owned relations; then scans unreachable object files. Findings are deterministic and detection-only: ForgeFS never silently repairs catalog rows — and never silently deletes objects either.

## Why it is agent-native

- **Pinned reasoning:** each session sees one base OID, not a moving checkout.
- **No silent clobber:** publication is CAS; losing writers fork.
- **No lost work on refusal:** a losing CAS forks the completed contribution and retargets the session to it (I18). No failure path silently discards staged work.
- **Reasoning-conflict detection:** observed `path → OID` assumptions are validated at checkin.
- **Capability isolation:** authority is `(operation, resource)` and attenuation only shrinks it.
- **Typed refs:** heads/forks, conflicts, and sealed tags cannot be type-confused.
- **Crash consistency:** objects become durable before one atomic metadata publication/session transition.
- **Fail-closed corruption:** malformed graph edges are corruption, never ordinary merge conflicts.
- **Trusted releases:** verification bypasses caches and validates tag → snapshot → commit → typed content/Contribution graph, including the exact versioned signed provenance manifest, against this forge's trusted seal key. Earlier content-only direct-map manifests remain verifiable without skipping the typed graph walk.
- **Small trusted core:** CAS + canonical objects + metadata transactions + capabilities + deterministic integration.

## A read is not a plain read

A read through a session is three things at once: a resolution against the pinned base plus the
overlay, a catalog lookup, and an **observation** — the `path → OID` fact that I9 re-checks at
checkin. That third part is what makes reads interesting, and it is why the read path has its own
machinery in `crates/forge-store/src/meta.rs`:

- **Catalog reads do not take the write mutex.** `get_ref`, `get_namespace`, `list_refs`, `list_namespaces`, `list_mounts`, `overlay_list`, `observations`, `intro_get`, `get_seal`, `get_seal_pub` and `reflog` run on a lazily filled pool of `SQLITE_OPEN_READONLY` + `query_only=1` connections. WAL already exists so readers need not queue behind the writer; until #315 the process-local write mutex put them back in the queue anyway. No pool member sets `journal_mode` or `synchronous`, so no pool member can weaken the durability contract; anything inside an explicit transaction stays on the write connection; a read-only `Meta` has no pool at all; and a failed pool open falls back to the write connection rather than failing the read.
- **A re-read that saw the same thing writes nothing.** `observe()` skips the INSERT when `(kind, oid)` for that path is unchanged. I9 is exactly as strict as before — the observation set still holds what the session read — but repeating a read no longer dirties a row to store a value already in it.

Both changes are semantics-preserving, which is why the I9 evidence row in the INVARIANTS.md
traceability table did not move.

## Measured, not claimed

Every number below came out of `forge bench` on the machine in the environment line, five
independent fresh repositories per configuration, medians as [`docs/BENCH.md`](docs/BENCH.md)
requires. They are **regression signals for this hardware**, not a "fastest filesystem" claim.
Controlled-hardware comparison (W7) owns that claim and does not exist yet.

```bash
cargo run --release --locked -p forge-cli -- bench --agents 32 --shared 16
```

Environment, emitted verbatim by `scripts/forge-env-line.sh`:

```text
forgefs commit:        63fa09852a0c60edf7cea1c5a1813d1410b74dc3
build profile:         release
forge --version:       forge 0.1.0
rustc:                 rustc 1.97.0 (2d8144b78 2026-07-07)
command line:          forge bench --agents 32 --shared 16 --scratch <fresh>
worker count:          64
cpu model:             AMD EPYC 9454 48-Core Processor
cpu logical cores:     4
ram:                   7.8 GiB (8343212032 bytes)
os:                    Debian GNU/Linux 12
kernel:                Linux 6.16.9+
arch:                  x86_64
filesystem:            ext2/ext3            # df -T reports ext4 on /dev/vdd
storage device:        /dev/vdd[/workspace]
sqlite journal_mode:   WAL
sqlite synchronous:    FULL (declared; Meta::open fails closed without it, docs/RECOVERY.md)
macos fullfsync:       n/a (Linux fsync path, docs/RECOVERY.md)
run class:             cold
repetition:            1..5 (fresh repository per invocation)
```

Median of 5 runs at `--agents 32 --shared 16`, default `--workers 64`:

| Measurement | Median of 5 |
|---|---|
| W1 serial checkin, one agent at a time (true op latency) | p50 **4.75 ms** |
| W1 private checkins, 32 concurrent | **647 ops/s**, p50 41.87 ms, p95 46.43 ms, p99/max 48.21 ms |
| W2 shared-ref stampede, 16 writers | p50 7.20 ms, p95 9.07 ms, p99 9.41 ms |
| W2 outcome, in all five runs | `updated=1 forked=15` |
| Tag verification | 0.003 s |
| Lifetime durability barriers | `puts=321 fsync_file=321 fsync_dir=828`, 0.20 s of barrier time |
| Lifetime SQLite | `txn_count=226 lock_acquires=994 busy=0 denied=0`; stale=0, conflict=0 |

Two honest readings of that table.

**The floor is a durability barrier, not a lock.** A checkin is `write → fsync(file) → exclusive
publish → fsync(parent directory)`, and one uncontended checkin costs 4.75 ms on this disk. That is
the number to beat, and fsync dominates it.

**A 41.87 ms p50 at 32 agents is oversubscription, not the storage engine.** The default
`--workers 64` puts 64 OS workers on 4 logical cores. Re-running the identical workload at
`--workers 4` — same five-repetition protocol, same box — moves throughput from 647 to
**674 ops/s** (the same, within run-to-run noise) while p50 drops from 41.87 ms to **5.62 ms**,
p99 from 48.21 ms to 7.42 ms, shared-stampede p50 from 7.20 ms to 2.72 ms, and whole-run SQLite
lock wait from **1.16 s to 38.7 ms**. Throughput here is barrier-bound; the tail at the default
worker count is convoy wait from oversubscribing the CPU. Match `--workers` to your cores before
drawing a conclusion about ForgeFS.

Two things that table deliberately does **not** say. `forge bench` and `forge stats --json` report
**cumulative process-lifetime** counters spanning init, both workloads, merge/seal, verification and
`fsck`; they are not per-operation measurements. Object-byte accumulation is uninstrumented and
prints the literal `bytes=unavailable`. The per-checkin cost mix
(`hash + encode + fsync_file + fsync_dir + sqlite_wait + sqlite_txn`) is reported as `unavailable`
and must not be reconstructed by dividing lifetime totals by a checkin count. `docs/BENCH.md` owns
those boundaries; `forge stats --json` is a counter surface, not a benchmark protocol, and says so
in its own `note` field.

The optimization order stays conservative: measure the durability barrier against the SQLite wait;
coalesce only provably safe barriers with crash tests green; split metadata readers from the single
writer only when profiles justify it (#315 was that, and only that); exploit canonical sorted trees
before inventing a new index or tree format; add chunking when large-file workloads demonstrate the
need.

See [`docs/BENCH.md`](docs/BENCH.md) for the reproducible workload and reporting protocol, and
[`docs/RECOVERY.md`](docs/RECOVERY.md) for the exact object + SQLite crash-durability contract.

## What ForgeFS does not do yet

A README that names its limits is worth more than one that does not.

- **Garbage collection reclaims, under a stated precondition.** `forge gc --dry-run` computes the reclaimable set and `forge gc --collect --min-age-secs <n>` unlinks it (roots = refs ∪ live session pins ∪ per-mount pinned bases ∪ staged overlay blobs ∪ observations ∪ landmarks ∪ seals ∪ unresolved forks); `forge abandon` retires a fork or session so it stops being a root. Soundness rests on a precondition the collector cannot prove for itself: no writer may take longer than `--min-age-secs` between putting an object and publishing a root that names it. Hence the hard 60s floor, the 4096-object batch cap, and the latency measurement in the concurrent soak (I23, `docs/GC.md`). A crash between object publication and metadata CAS still leaves durable orphans — safe, and now reclaimable.
- **No blob chunking, so one blob must fit in memory several times over.** `LocalBlobStore::put` takes `&[u8]`, `get` returns `Vec<u8>`, and a write also transiently holds the canonical encoding of the same bytes. Sampling `VmHWM` while writing single blobs measured peak RSS at **3.05x** the blob for 128 MiB, **3.02x** for 256 MiB and **3.01x** for 512 MiB; reading a 512 MiB blob back peaked at **3.01x** and returned byte-identical content. Treat **RAM/3** as the practical ceiling for a single blob. `forge write` already warns above 64 MiB (`forge: warning blob <n> bytes > 64MiB`).
- **Single node.** `forge serve` binds a `UnixListener`. There is no replication, no remote transport and no multi-host consensus. ForgeFS is a substrate for many agents on one machine.
- **Per-checkin cost attribution is unavailable**, as above. Do not redesign storage or concurrency on an incomplete attribution.
- **W6 (large tree walk/update) and W7 (Git comparator) have no published results.** Do not infer tree scaling from W1/W2, and do not quote a ForgeFS-versus-Git ratio until the durability-equivalence gate in `docs/BENCH.md` is satisfied.
- **Real process-kill crash evidence is still gated on #147.** A deterministic failpoint and an OS `SIGKILL` are different evidence classes, and `docs/BENCH.md` refuses to conflate them.
- **`forge checkin` publishes one mount at a time.** Each read-write mount carries its own pinned base (I19), so `forge checkin --ns <ns> --mount /path` folds that mount's overlay onto that mount's base and CASes the ref that mount names. A session that wrote through several mounts drains them one `--mount` at a time; `updated` and `forked` are progress and may leave another mount staged. What a bare `forge checkin` will never do is answer `noop` with exit 0 over work the session still holds — it refuses with exit 1 and names the mounts instead (#326, I22), so `noop` does mean the session is empty and `abandon` agrees.
- **A `--rw` mount on a protected ref accepts writes that `checkin` then refuses.** `main` is protected, so `forge mount /w --rw ref:main` succeeds and `forge write /w/...` is accepted, but `forge checkin --mount /w` exits 1 with `ref main is protected` and `forge abandon session` refuses because work is staged. The only exit is `abandon session --discard-staged`, which destroys the work. I20 refuses a `--rw oid:` mount and a ref not holding a commit for exactly this reason; a protected ref is the shape it does not yet check. Until it does, do not mount a protected ref `--rw`. `crates/forge-api/tests/model_composition.rs` reproduces it.

## Local validation

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
bash scripts/release-gate.sh target/debug/forge
bash scripts/cli-abi-conformance.sh target/debug/forge
cargo run --release --locked -p forge-cli -- bench --agents 32 --shared 16
```

At commit `63fa098` on the box above those produce: `cargo fmt` clean; clippy clean under
`-D warnings`; **292 tests passed, 0 failed**;
`release-gate: PASS - forge 0.2.1 sealed and verified itself as v0.2.1`; and
`abi rows=30 blocking=29 known_failing=0 unexercised=1 blocking_failures=0`.

CI runs the workspace on the pinned current Rust toolchain, Rust 1.89 MSRV, and a concurrent-agent
smoke workload, and compiles every `fuzz/` target with a 60-second smoke each.
[`CLI_ABI.md`](CLI_ABI.md) is the machine contract: automation keys on exit codes — `4` is a stale
observation or merge conflict, `2` is corruption or a sealed-state violation — never on stderr
wording.

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object IDs and structured errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical typed objects and deterministic tree COW |
| `forge-store` | Crash-durable write-once CAS + atomic SQLite metadata, with a read-only connection pool serving catalog reads |
| `forge-cap` | `(operation, resource)` macaroon-style capabilities |
| `forge-ns` | Session mounts and overlay resolution |
| `forge-merge` | DAG merge bases, deterministic 3-way merge, Conflict objects |
| `forge-api` | Small public facade split by concern: `repository`, `authority`, `workspace`, `refs`, `integration`, `import`, `export`, `fsck`, `stats`, `serve` |
| `forge-cli` | `forge`; requires explicit `--cap` / `FORGE_CAP` |

## Many-agent direction

ForgeFS stays the **truth/convergence layer**, not an agent scheduler. Orchestrators can add stable task identity, durable inbox/outbox handoffs, dashboards, and role workflows on top of immutable contribution/handoff receipts plus tiny CASed indexes. That keeps the storage core simple enough to audit while scaling to many independent agents.
