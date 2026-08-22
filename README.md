# ForgeFS

A **concurrency and provenance substrate for autonomous agents**.

Not another POSIX filesystem. Not S3 copy-in/copy-out. Bytes are immutable and content-addressed. Named refs move by compare-and-swap. Sessions pin a snapshot. Checkin fails on **overlapping writes and stale reads**. The official release is a cryptographically sealed tag.

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

## 60-second thesis

```bash
cargo test --workspace
cargo run -p forge-cli -- init ./demo
# no ambient root: pass the cap file init printed
CAP=./demo/.forge/keys/root.cap
INT=./demo/.forge/keys/integrator.cap

A=$(cargo run -q -p forge-cli -- --dir ./demo --cap $CAP session open --from=main)
B=$(cargo run -q -p forge-cli -- --dir ./demo --cap $CAP session open --from=main)
cargo run -p forge-cli -- --dir ./demo --cap $CAP write --ns $A /a.txt --text "alice"
cargo run -p forge-cli -- --dir ./demo --cap $CAP write --ns $B /b.txt --text "bob"
cargo run -p forge-cli -- --dir ./demo --cap $CAP checkin --ns $A -m a
cargo run -p forge-cli -- --dir ./demo --cap $CAP checkin --ns $B -m b
# merge + seal (integrator cap)
cargo run -p forge-cli -- --dir ./demo --cap $INT merge --into=main --from=heads/agents/anon/$A
cargo run -p forge-cli -- --dir ./demo --cap $INT seal main --tag v1.0 --attest
cargo run -p forge-cli -- --dir ./demo --cap $CAP verify v1.0
```

Two agents, one forge, no cloud. If they both touch the same path you get a **conflict object**. If one shipped a new `/x` while the other still reasoned about the old `/x`, you get **stale observation** — even when the second agent only writes `/y`.

See [INVARIANTS.md](INVARIANTS.md) for the 15-line correctness model.

## Local (no Docker)

```bash
cargo test --workspace
```

## Layout

| Crate | Role |
|---|---|
| `forge-types` | ObjectId, errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical CBOR objects, tree COW |
| `forge-store` | Write-once CAS + SQLite transactions |
| `forge-cap` | `(op, resource)` macaroons; attenuation only shrinks |
| `forge-ns` | Mount tables |
| `forge-merge` | 3-way merge, conflict objects |
| `forge-api` | Sessions, checkin, seal, serve |
| `forge-cli` | `forge` (requires `--cap` / `FORGE_CAP`) |
