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
```

Two agents, one forge, no cloud and no shared mutable checkout. If both change the same path, ForgeFS emits a **Conflict object**. If Alice changes `/x` after Bob observed its old OID, Bob's later checkin fails with **stale observation** even when Bob only writes `/y`. If writers race one mutable ref, one wins and every loser becomes an explicit fork.

See [INVARIANTS.md](INVARIANTS.md) for the compact correctness contract.

## Why it is agent-native

- **Pinned reasoning:** each session sees one base OID, not a moving checkout.
- **No silent clobber:** publication is CAS; losing writers fork.
- **Reasoning-conflict detection:** observed `path → OID` assumptions are validated at checkin.
- **Capability isolation:** authority is `(operation, resource)` and attenuation only shrinks it.
- **Typed refs:** heads/forks, conflicts, and sealed tags cannot be type-confused.
- **Crash consistency:** objects become durable before one atomic metadata publication/session transition.
- **Fail-closed corruption:** malformed graph edges are corruption, never ordinary merge conflicts.
- **Trusted releases:** verification bypasses caches and validates tag → snapshot → commit → tree/provenance using this forge's trusted seal key.
- **Small trusted core:** CAS + canonical objects + metadata transactions + capabilities + deterministic integration.

## Speed model — evidence over slogans

Puts are deliberately durable: **write → fsync(file) → exclusive publish → fsync(parent directory)**. Private agents create immutable objects independently; SQLite serializes only short metadata transitions. A shared-ref stampede becomes **1 update + N−1 forks**, never lost work.

```bash
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

Existing local and GitHub-hosted results are regression signals, not a universal "fastest filesystem" claim. Controlled-hardware comparison owns that claim. The optimization order is:

1. measure durability-barrier time vs SQLite wait;
2. coalesce only provably safe durability barriers with crash tests green;
3. split metadata readers from the single writer only if profiles justify it;
4. exploit canonical sorted trees before inventing a new index/tree format;
5. add chunking when large-file workloads demonstrate the need.

## Local validation

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

CI runs workspace tests on the pinned current Rust toolchain, the Rust 1.89 MSRV, and a concurrent-agent smoke workload.

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object IDs and structured errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical typed objects and deterministic tree COW |
| `forge-store` | Crash-durable write-once CAS + atomic SQLite metadata |
| `forge-cap` | `(operation, resource)` macaroon-style capabilities |
| `forge-ns` | Session mounts and overlay resolution |
| `forge-merge` | DAG merge bases, deterministic 3-way merge, Conflict objects |
| `forge-api` | Capability-checked sessions, checkin, merge, seal, verify, serve |
| `forge-cli` | `forge`; requires explicit `--cap` / `FORGE_CAP` |

## Many-agent direction

ForgeFS stays the **truth/convergence layer**, not an agent scheduler. Inspired by durable swarm systems, orchestrators can add stable task identity, inbox/outbox workflows, handoffs, and dashboards on top of immutable contribution/handoff receipts plus tiny CASed indexes. That preserves a small auditable core while giving thousands of agents durable coordination primitives.
