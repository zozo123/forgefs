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

## Speed model (honest)

Puts are **durable**: write → fsync(file) → exclusive link → fsync(dir). That is ~1 ms per object on SSD, by design (crash-safe). Checkin cost is that times (1 blob + directories on the COW spine) plus one SQLite `BEGIN IMMEDIATE`.

Private agents do **not** fight over `main`. Each owns a ref row. SQLite serializes the tiny CAS txn; object bytes do not go through SQLite. A shared-ref stampede becomes **1 update + N forks**, not a lock convoy.

```bash
cargo run -p forge-cli -- bench --agents 32 --shared 16
```

Measured on this Mac (debug, APFS, durable fsync) @ `f5b6617` lineage:

| Workload | Result |
|---|---|
| Serial checkin (grant+session+write+CAS) | p50 **38 ms** |
| 32 private agents | **32/32 Updated**, **35 Hz**, wall 0.9 s |
| 128 private | **128/128**, **42 Hz**, wall 3.1 s |
| 256 private | **256/256**, **40 Hz**, wall 6.4 s |
| 16/32/64 shared-ref stampede | **1 Updated + N-1 Forked** every time |
| verify after seal | **1–10 ms** |

Throughput is ~40 durable checkins/s because each object put fsyncs the file *and* its directory (I4). p50 under load ≈ wall clock: threads convoy on fsync + SQLite `BEGIN IMMEDIATE`, they do **not** clobber. Scale-out is more private refs, not a faster `main`.

## Local (no Docker)

```bash
cargo test --workspace
cargo run -p forge-cli -- bench --agents 32 --shared 16
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
