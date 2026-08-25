# ForgeFS invariants

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

```
objects     write-once; ObjectId = BLAKE3(canonical file bytes)
refs        only mutable surface; every move is CAS(expected → new)
session     (cap, ns, pinned_base_oid, observation_set)
read        resolved against the pinned base plus the overlay, never a live ref
checkin     overlay folded onto the pinned base, then CAS that oid
conflict    first-class object, never only a string
seal        signed snapshot; tag is frozen; verify rereads durable bytes
cap         (operation, resource); attenuation ⊆ parent
```

| ID | Rule |
|---|---|
| I1 | Decode(encode(x)) is encode(x). Non-canonical bytes, unknown fields, and unknown VERSION 1 type bytes are Corrupt. |
| I2 | One logical object ⇒ one byte string ⇒ one ObjectId. |
| I3 | Put is idempotent iff bytes match; never overwrite. |
| I4 | A committed ref implies fsynced object bytes and every directory edge needed to reach them; visibility alone is never a durability proof. |
| I5 | Refs move only expected→new. Lost CAS forks or denies. Protected refs deny. |
| I6 | Ref + reflog (+ seal) commit together. |
| I7 | tags/ conflicts/ heads/ are typed, not naming conventions. |
| I8 | session.open pins a base OID. Reads through a session resolve against that base and its overlay, never a live ref another agent can move. Checkin CASes that oid, never a moving head. |
| I9 | Every read records what it saw at a path: a blob id, a directory's tree id, or its absence. Silence is not a recorded read. Stale observations fail checkin even on disjoint writes. |
| I10 | Checkin is a Contribution (`0x06`), not a loose message. A missing `Commit.contrib` is the canonical historical `None`; a present edge must verify as a Contribution. Every current-version seal manifest includes each Contribution reachable from its commit and binds the entry to the receipt's immutable agent. |
| I11 | Overlap is a Conflict object. |
| I12 | Merge order comes only from the commit parent DAG and real merge-bases. `Commit.ts` and `Contribution.ts` are advisory metadata, never causal order. |
| I13 | Authority(c+d) ⊆ Authority(c). Holder attenuates without the root secret. |
| I14 | No ambient root. Namespace ID is not a capability. |
| I15 | verify/fsck reread durable bytes and this forge's seal key. Seal verification proves the exact provenance scope for its manifest version; the legacy content-only shape remains readable but still triggers a complete typed Contribution-graph walk. Missing, extra, malformed, or wrongly typed edges fail closed. |
| I16 | Tree names are exact UTF-8 bytes: no Unicode normalization or case folding occurs in core identity. |
| I17 | Repository VERSION gates immutable decoding and is independent of SQLite schema version. Unknown future values fail closed; metadata migrations never rewrite objects or ObjectIds. |
| I18 | A refused checkin never destroys staged work. A losing CAS forks the completed contribution and retargets the session to it; no failure path silently discards work. A fork stays a reclamation root until it is explicitly resolved -- merged, or retired by `abandon`, which is a deliberate act and not a failure path. |

A composed Unicode name and its canonically equivalent decomposed spelling are distinct ForgeFS entries if their UTF-8 byte strings differ. Likewise `Foo` and `foo` are distinct. Export adapters must detect target-filesystem collisions and fail rather than silently normalize, fold, or overwrite names. `export_tar` refuses a directory whose sibling names collide under case folding or Unicode canonical equivalence and names both spellings with their bytes; `ExportOptions::allow_name_collisions` (`forge export --allow-name-collisions`) is the deliberate per-call opt-out for a case-sensitive destination, never a default and never inferred from the exporting host.

## Executable evidence

The public cross-cutting seam is table-tested in
[`api_contract.rs`](crates/forge-api/tests/api_contract.rs). Mechanism-specific
proofs stay separate so a real race, process, crash, or filesystem boundary is
not diluted into a mock:

| Invariants | Production owner | Primary evidence |
|---|---|---|
| I1, I2, I10, I17 | `forge-core`, `forge-store/graph.rs`, `forge-store/meta.rs`, `forge-api/import.rs`, `integration.rs`, `repository.rs` | `golden_object_ids.rs`, `adversarial_canonical.rs`, `provenance.rs`, `checkin_contribution.rs`, `typed_graph.rs`, `api_contract.rs`, `bootstrap_contract.rs`, `schema_migrations.rs`, `schema_migration_fixtures.rs`, `schema_migration_objects.rs`, `testdata/schema/README.md`, `property_canonical.rs`, `fuzz/tree_name` |
| I3, I4, I6 | `forge-store`, `repository.rs` | `meta_invariants.rs`, `session_atomicity.rs`, `barrier_fault_injection.rs`, `cross_process_put.rs`, `cli_sigkill.rs`, `docs/RECOVERY.md` |
| I5, I7, I8 | `forge-store/meta.rs`, `forge-api/workspace.rs`, `refs.rs` | `api_contract.rs`, `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `fsck_concurrent_fork.rs`, `fuzz/ref_name` |
| I9 | `forge-api/workspace.rs` | `api_contract.rs`, `e2e_concurrent.rs` |
| I18 | `forge-api/workspace.rs`, `forge-api/gc.rs`, `forge-store/meta.rs` | `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `gc_and_abandon.rs`, `docs/GC.md` |
| I11, I12 | `forge-merge`, `forge-api/integration.rs` | `api_contract.rs`, `merge_bases.rs`, `clock_causality.rs`, `show_conflict.rs`, `cli_merge_race.rs`, `rename_characterisation.rs`, `property_merge_symmetry.rs` |
| I13, I14 | `forge-cap`, `forge-api/authority.rs` | `api_contract.rs`, `capability_boundary.rs`, `p0_authority_history.rs`, `cli_cross_cell.rs`, `property_attenuation.rs`, `fuzz/cap_token` |
| I15 | `forge-api/integration.rs`, `fsck.rs`, `forge-store/graph.rs`, `forge-store/meta.rs` | `api_contract.rs`, `typed_graph.rs`, `seal_trust_root.rs`, `trust_boundary.rs`, `cli_recovery_and_corruption.rs` |
| I16 | `forge-core/tree.rs`, `forge-api/export.rs` | `path_identity.rs`, `export_long_names.rs`, `export_name_collisions.rs`, `fuzz/tar_roundtrip` |

Tests are named after these IDs or state the invariant in a one-line rationale.
A PR that cannot name an invariant does not merge.

Two evidence shapes back these rows. Deterministic property tests state an
invariant as algebra and drive it from a seeded generator (no property-testing
dependency; the seed is printed with any failure, so a counterexample is
reproducible):

| Property test | Statement |
|---|---|
| `forge-core/tests/property_canonical.rs` | decode(encode(x)) is x, re-encoding reproduces the same bytes, and the encoding does not depend on incidental input order (I1, I2) |
| `forge-merge/tests/property_merge_symmetry.rs` | the merged TREE and the conflicting-path set do not depend on which side is `ours` (I12) |
| `forge-cap/tests/property_attenuation.rs` | appending caveats can only shrink the reachable (op, ref, clock) set, and the attenuated token still verifies (I13) |

The `fuzz/` targets cover the same boundaries with untrusted bytes: typed
decoders (`object_decode`), the capability parser (`cap_token`), daemon framing
and dispatch (`protocol_frame`), the tree-name and ref-name grammars
(`tree_name`, `ref_name`), and the tar export/import round trip
(`tar_roundtrip`). CI compiles every target and smokes each for 60 seconds.
