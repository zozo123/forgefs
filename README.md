# ForgeFS

A **concurrency, isolation, and provenance substrate for autonomous agents**.

ForgeFS is not another POSIX filesystem and not S3 copy-in/copy-out. Bytes are immutable and content-addressed. Agents reason from pinned snapshots. Named refs publish by compare-and-swap. Checkin detects both **write conflicts** and **stale observations**. Losing races fork instead of clobbering work. Releases are cryptographically sealed and verified from durable bytes.

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

## 60-second two-agent path

```bash
cargo test --workspace --all-targets --locked
cargo run -p forge-cli -- init ./demo

ROOT=./demo/.forge/keys/root.cap
INT=./demo/.forge/keys/integrator.cap

ALICE=$(cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT grant \
  --ops read,write,branch --ref 'main,heads/agents/alice/*' --agent alice)
BOB=$(cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT grant \
  --ops read,write,branch --ref 'main,heads/agents/bob/*' --agent bob)

A=$(cargo run -q -p forge-cli -- --dir ./demo --cap "$ALICE" session open --from=main)
B=$(cargo run -q -p forge-cli -- --dir ./demo --cap "$BOB" session open --from=main)

cargo run -q -p forge-cli -- --dir ./demo --cap "$ALICE" write --ns "$A" /a.txt --text alice
cargo run -q -p forge-cli -- --dir ./demo --cap "$BOB"   write --ns "$B" /b.txt --text bob
cargo run -q -p forge-cli -- --dir ./demo --cap "$ALICE" checkin --ns "$A" -m alice
cargo run -q -p forge-cli -- --dir ./demo --cap "$BOB"   checkin --ns "$B" -m bob

cargo run -q -p forge-cli -- --dir ./demo --cap $INT merge --into=main --from="heads/agents/alice/$A"
cargo run -q -p forge-cli -- --dir ./demo --cap $INT merge --into=main --from="heads/agents/bob/$B"
cargo run -q -p forge-cli -- --dir ./demo --cap $INT seal main --tag v1.0 --attest
cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT verify v1.0
cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT fsck --full
```

Two agents, one forge, no shared mutable checkout. Same-path edits become a **Conflict object**. If one agent changes `/x` after another observed its old OID, the stale agent's later checkin fails even when it only writes `/y`. If writers race one mutable ref, one wins and every loser becomes an explicit fork.

See [INVARIANTS.md](INVARIANTS.md) for the compact correctness contract and
[AGENTS.md](AGENTS.md) for the contributor architecture and change rules.

## Integrity is a product surface

```bash
# refs, seals, namespaces, mounts, observations, and reachable typed object closure
cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT fsck

# additionally prove SQLite structure/catalog relations and scan every object file
cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT fsck --full

# machine-readable report for CI/agents
cargo run -q -p forge-cli -- --dir ./demo --cap $ROOT fsck --full --json
```

`fsck` is read-only. It bypasses hot object caches, rehashes durable bytes, verifies expected type on every graph edge, checks sealed tags against the trusted local seal key, validates namespace/live-ref/mount state, and bounds graph work. `fsck --full` also runs SQLite integrity checking from one coherent read transaction; proves the schema ledger, ref/reflog terminal state and chain, seal/tag/landmark closure, provenance rows, and namespace-owned relations; then scans unreachable object files. Findings are deterministic and detection-only: ForgeFS never silently repairs catalog rows.

## Why it is agent-native

- **Pinned reasoning:** each session sees one base OID, not a moving checkout.
- **No silent clobber:** publication is CAS; losing writers fork.
- **Reasoning-conflict detection:** observed `path → OID` assumptions are validated at checkin.
- **Capability isolation:** authority is `(operation, resource)` and attenuation only shrinks it.
- **Typed refs:** heads/forks, conflicts, and sealed tags cannot be type-confused.
- **Crash consistency:** objects become durable before one atomic metadata publication/session transition.
- **Fail-closed corruption:** malformed graph edges are corruption, never ordinary merge conflicts.
- **Trusted releases:** verification bypasses caches and validates tag → snapshot → commit → typed content/Contribution graph, including the exact signed provenance manifest, against this forge's trusted seal key.
- **Small trusted core:** CAS + canonical objects + metadata transactions + capabilities + deterministic integration.

## Speed model — evidence over slogans

Puts are deliberately durable: **write → fsync(file) → exclusive publish → fsync(parent directory)**. Private agents create immutable objects independently; SQLite serializes short metadata transitions. A shared-ref stampede becomes **1 update + N−1 forks**, never lost work.

```bash
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

Existing local and GitHub-hosted results are regression signals, not a universal "fastest filesystem" claim. Controlled-hardware comparison owns that claim. The optimization order is deliberately conservative:

1. measure durability-barrier time vs SQLite wait;
2. coalesce only provably safe durability barriers with crash tests green;
3. split metadata readers from the single writer only if profiles justify it;
4. exploit canonical sorted trees before inventing a new index/tree format;
5. add chunking when large-file workloads demonstrate the need.

See [`docs/BENCH.md`](docs/BENCH.md) for the reproducible workload/reporting
protocol and [`docs/RECOVERY.md`](docs/RECOVERY.md) for the exact object +
SQLite crash-durability contract.

## Local validation

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

CI runs the workspace on the pinned current Rust toolchain, Rust 1.89 MSRV, and a concurrent-agent smoke workload.

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object IDs and structured errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical typed objects and deterministic tree COW |
| `forge-store` | Crash-durable write-once CAS + atomic SQLite metadata |
| `forge-cap` | `(operation, resource)` macaroon-style capabilities |
| `forge-ns` | Session mounts and overlay resolution |
| `forge-merge` | DAG merge bases, deterministic 3-way merge, Conflict objects |
| `forge-api` | Small public facade over invariant-aligned repository, authority, workspace, refs, integration, import/export, fsck, and serve modules |
| `forge-cli` | `forge`; requires explicit `--cap` / `FORGE_CAP` |

## Many-agent direction

ForgeFS stays the **truth/convergence layer**, not an agent scheduler. Orchestrators can add stable task identity, durable inbox/outbox handoffs, dashboards, and role workflows on top of immutable contribution/handoff receipts plus tiny CASed indexes. That keeps the storage core simple enough to audit while scaling to many independent agents.
