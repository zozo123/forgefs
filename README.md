# ForgeFS

A content-addressable filesystem for concurrent agents and humans.

Bytes are immutable and addressed by BLAKE3. Named refs are the only mutable pointers. Colliding writes fork instead of clobbering. Authority is a capability. The official release of a project is a sealed, signed snapshot of `main`.

This is not Git, not IPFS, and not an S3 copy-in/copy-out workspace. It is designed so thousands of writers can check in and check out in parallel without silently overwriting each other.

## Run locally (no Docker, no network)

```bash
cargo test --workspace
cargo run -p forge-cli -- init ./demo
NS=$(cargo run -q -p forge-cli -- --dir ./demo session open --from=main)
cargo run -p forge-cli -- --dir ./demo write --ns "$NS" /hello.txt --text "hello"
cargo run -p forge-cli -- --dir ./demo checkin --ns "$NS" -m "hello"
# integrator publishes
cargo run -p forge-cli -- --dir ./demo --cap ./demo/.forge/keys/integrator.cap \
  merge --into=main --from=heads/agents/anon/$NS
cargo run -p forge-cli -- --dir ./demo --cap ./demo/.forge/keys/integrator.cap \
  seal main --tag v1.0 --attest
cargo run -p forge-cli -- --dir ./demo export tags/v1.0 -o /tmp/v1.0.tar
```

## Shape

| Crate | Role |
|---|---|
| `forge-types` | `ObjectId`, errors |
| `forge-core` | Canonical object encode/decode, tree COW |
| `forge-store` | Local CAS + SQLite refs/overlay |
| `forge-cap` | HMAC macaroons |
| `forge-ns` | Plan 9-style namespaces and mounts |
| `forge-merge` | 3-way merge, conflict objects |
| `forge-protocol` | Length-prefixed request ABI |
| `forge-api` | `Forge` library + `serve` |
| `forge-cli` | `forge` binary |

See the in-repo design: every write-once object lives under `.forge/objects/`, refs live in SQLite WAL, and `forge seal main --tag v1.0` freezes a tag without freezing the future of `main`.
