# ForgeFS contributor operating manual

Read this before changing the repository. ForgeFS is a small trust and
convergence core; reviewability is a product requirement.

## Document map

Each fact has one owner. Link to it instead of copying it:

| Document | Owns |
|---|---|
| `README.md` | Product goal, user path, crate map, and performance posture |
| `INVARIANTS.md` | Normative correctness rules and executable evidence |
| `FORMAT.md` | Frozen VERSION 1 object framing and canonical encodings |
| `CLI_ABI.md` | Stable CLI output, error, and exit-code contract |
| `docs/RECOVERY.md` | Crash-durability and recovery contract |
| `docs/BENCH.md` | Reproducible benchmark protocol and claim boundaries |
| `docs/CHUNKING.md` | Measured object-size ceiling and the chunked-file design decision |
| `docs/RELEASING.md` | Release, artifact, and verification procedure |
| `SECURITY.md` | Threat model, supported reporting path, and non-goals |

The code is authoritative when documentation and implementation disagree. Fix
the stale document in the same change.

## Product boundary

ForgeFS is the truth and convergence layer for independent agents:

- immutable, canonically encoded objects;
- tiny mutable refs published with compare-and-swap;
- capability-scoped namespaces pinned to one base;
- explicit Contribution and Conflict objects;
- deterministic integration; and
- sealed releases verified from durable bytes.

It is not an agent scheduler, a shared mutable checkout, a general POSIX
filesystem, a code-review system, or an eventually consistent object store.
Adapters and orchestrators belong above the core.

Prefer the smallest mechanism that makes an invariant executable. Do not add a
framework, trait layer, cache, index, or compatibility shim without a measured
or demonstrated need.

## Commit points and trust boundaries

Memorize these before editing a write path:

1. Object identity is BLAKE3 over canonical complete file bytes.
2. Object bytes and their directory edges are durable before metadata can name
   them.
3. A SQLite ref transaction is the visibility point. A lost expected value is
   an explicit fork, denial, or conflict; never a retry that hides an outcome.
4. A session reads and checks in from its pinned commit. Foreign read-only
   mounts may remain live so stale observations can be detected.
5. Capability verification precedes resource use. Namespace IDs and raw object
   IDs carry no authority.
6. Merge causality comes only from the commit-parent DAG. Timestamps are
   advisory.
7. Seal verification bypasses caches and rereads the durable typed graph using
   this cell's trusted public key.

The immutable VERSION 1 bytes are frozen. Any incompatible object change
requires an explicit repository VERSION transition and golden fixtures.
Metadata schema migration never rewrites immutable objects.

## Code map

Crate boundaries are trust boundaries:

| Area | Responsibility |
|---|---|
| `forge-types` | IDs, entry kinds, and stable structured errors |
| `forge-core` | Canonical typed objects and tree copy-on-write |
| `forge-store` | Durable write-once objects and atomic metadata |
| `forge-cap` | Capability verification and monotone attenuation |
| `forge-ns` | Namespace mounts, overlay resolution, and path rules |
| `forge-merge` | Merge bases, deterministic three-way merge, conflicts |
| `forge-protocol` | Bounded framed daemon protocol |
| `forge-api` | Capability-checked public facade and integrity operations |
| `forge-cli` | User/process boundary and CLI ABI |

`forge-api/src/lib.rs` is intentionally only the public facade and shared
state. Put implementation in the invariant-aligned module:

| Module | Boundary |
|---|---|
| `repository.rs` | Discovery, init/open, locking, keys, durability helpers |
| `authority.rs` | I13/I14 capability and namespace ownership |
| `workspace.rs` | I8/I9 sessions, mounts, observations, checkin |
| `refs.rs` | Typed refs, inbox, history, and ref/object resolution |
| `integration.rs` | I11/I12 merge and I15 seal/verify |
| `import.rs` / `export.rs` | Host adapter boundaries and TOCTOU/losslessness |
| `fsck.rs` | Durable typed-graph verification; never repair |
| `serve.rs` | Bounded daemon admission and protocol dispatch |
| `bench.rs` / `soak.rs` | Evidence, never correctness policy |
| `stats.rs` | Machine-readable process-lifetime counter document; evidence only |

Avoid sibling-module reach-through. Shared helpers should have one semantic
owner and the narrowest crate visibility required.

## Test rules

- Name the invariant in a test or its one-line rationale.
- Keep pure helper tests beside the helper.
- Keep the public cross-cutting contract in
  `crates/forge-api/tests/api_contract.rs`.
- Keep races, real subprocesses, SIGKILL, filesystem behavior, and crash
  recovery as real focused tests. Do not replace their mechanism with mocks.
- Every bug fix needs a regression that fails for the old behavior.
- Fault injection must be explicit, test-only, and absent from release control
  paths.
- Never make a performance claim from a correctness test or a process-lifetime
  counter.

## Change discipline

1. State the invariant and commit point affected.
2. Make behavior changes separately from mechanical moves.
3. Keep public API, object format, CLI ABI, and durability semantics unchanged
   unless the change explicitly updates their owning contract.
4. Fail closed on unknown types, versions, fields, paths, capabilities, and
   ambiguous outcomes.
5. Preserve caller-owned files. Temporary output must be sibling-written,
   verified, atomically published, and removed on failure.
6. Do not silently repair corruption.

Required validation:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
scripts/cli-abi-conformance.sh target/debug/forge
scripts/release-gate.sh target/debug/forge
```

CI also checks Rust 1.89, macOS durability paths, concurrent-agent smoke, fuzz
builds, dependency policy, and workflow/release gates. A green fast unit test is
not a substitute for the relevant gate.
