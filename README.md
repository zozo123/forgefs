# ForgeFS

A **concurrency, provenance, and convergence substrate for autonomous agents**.

Not another POSIX filesystem. Not S3 copy-in/copy-out. Bytes are immutable and content-addressed. Named refs move by compare-and-swap. Sessions pin a snapshot. Checkin fails on **overlapping writes and stale reads**. Lost CAS writers fork instead of clobbering. The official release is a cryptographically sealed tag.

**Immutable bytes. Explicit authority. Snapshot reasoning. Atomic publication. Loud conflicts. Verifiable releases.**

> Agents may be complex; shared truth must be boring.

## 60-second thesis

```bash
cargo test --workspace
cargo run -p forge-cli -- init ./demo
# no ambient root: pass an explicit capability
CAP=./demo/.forge/keys/root.cap
INT=./demo/.forge/keys/integrator.cap

A=$(cargo run -q -p forge-cli -- --dir ./demo --cap $CAP session open --from=main)
B=$(cargo run -q -p forge-cli -- --dir ./demo --cap $CAP session open --from=main)
cargo run -p forge-cli -- --dir ./demo --cap $CAP write --ns $A /a.txt --text "alice"
cargo run -p forge-cli -- --dir ./demo --cap $CAP write --ns $B /b.txt --text "bob"
cargo run -p forge-cli -- --dir ./demo --cap $CAP checkin --ns $A -m a
cargo run -p forge-cli -- --dir ./demo --cap $CAP checkin --ns $B -m b

# deterministic integration + trusted release
cargo run -p forge-cli -- --dir ./demo --cap $INT merge --into=main --from=heads/agents/anon/$A
cargo run -p forge-cli -- --dir ./demo --cap $INT seal main --tag v1.0 --attest
cargo run -p forge-cli -- --dir ./demo --cap $CAP verify v1.0
```

Two agents, one forge, no cloud. If they touch the same path you get a **Conflict object**. If one changed `/x` while another still reasoned about the old `/x`, the second gets **StaleObservation** even if it only writes `/y`.

For hundreds or thousands of agents, the model stays the same: each agent works on a pinned namespace/private ref; immutable objects are written in parallel; only tiny ref/session metadata transitions serialize. Shared-ref races become **one winner + explicit forks**, never silent overwrite.

See [INVARIANTS.md](INVARIANTS.md) for the correctness model and [THREAT_MODEL.md](THREAT_MODEL.md) for the explicit security boundary.

## Many-agent architecture

```text
 agent A ---- pinned namespace ----\
 agent B ---- pinned namespace -----+--> immutable Merkle objects
 agent C ---- pinned namespace ----/              |
                                                  v
                                      commit / contribution
                                                  |
                                   tiny atomic CAS/ref publish
                                      /           |          \
                                  updated       forked     conflict
                                      \           |          /
                                       deterministic integrator
                                                  |
                                           sealed release
```

Orchestrators, sandboxes, model runtimes, and future swarm handoff layers are **clients** of ForgeFS, not part of the filesystem core.

## Speed model

Object publication is crash-durable: write temp -> fsync(file) -> exclusive link -> fsync(destination directory). Metadata publication uses small SQLite `BEGIN IMMEDIATE` transactions. Object bytes do not flow through SQLite.

```bash
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

A recent Ubuntu CI sample on the transactional #77 lineage produced:

| Workload | Observed sample |
|---|---:|
| Serial durable checkin | p50 **7.87 ms**, p95 **9.69 ms** |
| 32 private agents | **32/32 updated**, **278 checkins/s**, wall **0.115 s** |
| Loaded private-agent latency | p50 **96.84 ms**, p95 **110.62 ms** |
| 16-agent shared-ref stampede | **1 updated + 15 forked**, wall **23 ms** |
| Merge + seal | **190 ms** |
| Verify sealed release | **12 ms** |

CI runners vary, so these are samples rather than promises. The important semantic result is invariant: private writers scale through immutable object work, while a shared-ref stampede converges without clobbering.

## Security boundary

Capabilities protect untrusted clients that use the **Forge API/protocol**. `Forge::store` is private, but `forge-store` is still a trusted systems-layer crate. An OS principal or native component with direct read/write access to `.forge` is an administrator, not an untrusted capability client.

For adversarial agent code, do not expose `.forge` inside the sandbox. Give the sandbox only the Forge socket/API and a least-authority `(operation x resource)` capability. See [THREAT_MODEL.md](THREAT_MODEL.md).

## Local development

```bash
cargo test --workspace
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object IDs, entry kinds, structured errors |
| `forge-core` | Canonical objects and Merkle tree COW |
| `forge-store` | Write-once CAS + transactional trusted metadata layer |
| `forge-cap` | `(operation, resource)` macaroons; attenuation only shrinks |
| `forge-ns` | Namespace/mount resolution |
| `forge-merge` | DAG merge-bases, 3-way merge, Conflict objects |
| `forge-api` | Capability-checked sessions, checkin, merge, seal, serve |
| `forge-cli` | `forge`; explicit `--cap` / `FORGE_CAP` |

## Direction

ForgeFS deliberately does **not** become an agent scheduler. The high-value extensions are filesystem primitives: `fsck/doctor`, actionable conflict resolution, contribution/observation receipts, tiny inbox refs for agent handoffs, GC/leases, and measured durability/throughput optimization.
