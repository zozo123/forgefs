# ForgeFS invariants

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

```
objects     write-once; ObjectId = BLAKE3(canonical file bytes)
refs        only mutable surface; every move is CAS(expected → new)
session     (cap, ns, mounts, observation_set)
mount       (path, spec, mode, pinned_base_oid) -- rw mounts pin, ro mounts live
read        resolved against ITS MOUNT's pinned base plus the overlay,
            never a live ref
checkin     one mount's overlay folded onto that mount's pinned base,
            then CAS that mount's ref from that oid
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
| I8 | session.open pins a base OID. Reads through a session resolve against a pinned base -- per mount, see I19 -- and its overlay, never a live ref another agent can move. Checkin CASes that oid, never a moving head. |
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
| I19 | Every read-write mount carries its OWN pinned base, recorded when the mount is taken. Reads, `ls`, observation checks and checkin through a mount all resolve against that mount's pin, so the mount MODE never decides which ref you read and a mount of one ref never answers out of another's tree. Checkin folds one mount's overlay onto that mount's pin and CASes the ref THAT MOUNT names, from that pin, re-pinning only that mount. A read-only mount carries no pin and resolves live on purpose: that is what makes cross-mount staleness detectable (I9). Re-mounting a path whose overlay was staged against a different spec, or demoting one that holds staged work, is refused, never silently retargeted. |
| I20 | A mount that accepts a write has a verb that can publish it. A read-write mount must name a ref holding a commit, so an immutable `oid:` spec and a non-commit ref are refused at mount time rather than accepting writes no capability and no verb could ever publish. |
| I21 | A session holding write authority over a mount's base can always reach a terminal state for it: publish, fork, or `abandon`. No sequence of authorised reads may leave every checkin refused. A read-write mount is validated against the same pin its own reads came from, so an authorised read can never make the session's work unpublishable; a refusal a re-read cannot clear must name the mount that caused it, not only the observation. |

A composed Unicode name and its canonically equivalent decomposed spelling are distinct ForgeFS entries if their UTF-8 byte strings differ. Likewise `Foo` and `foo` are distinct. Export adapters must detect target-filesystem collisions and fail rather than silently normalize, fold, or overwrite names. `export_tar` refuses a directory whose sibling names collide under case folding or Unicode canonical equivalence and names both spellings with their bytes; `ExportOptions::allow_name_collisions` (`forge export --allow-name-collisions`) is the deliberate per-call opt-out for a case-sensitive destination, never a default and never inferred from the exporting host.

## Executable evidence

The public cross-cutting seam is table-tested in
[`api_contract.rs`](crates/forge-api/tests/api_contract.rs). Mechanism-specific
proofs stay separate so a real race, process, crash, or filesystem boundary is
not diluted into a mock:

| Invariants | Production owner | Primary evidence |
|---|---|---|
| I1, I2, I10, I17 | `forge-core`, `forge-store/graph.rs`, `forge-store/meta.rs`, `forge-api/import.rs`, `integration.rs`, `repository.rs` | `golden_object_ids.rs`, `adversarial_canonical.rs`, `provenance.rs`, `checkin_contribution.rs`, `typed_graph.rs`, `api_contract.rs`, `bootstrap_contract.rs`, `schema_migrations.rs`, `schema_migration_fixtures.rs`, `schema_migration_objects.rs`, `testdata/schema/README.md`, `property_canonical.rs`, `large_blob_memory.rs`, `fuzz/tree_name` |
| I3, I4, I6 | `forge-store`, `repository.rs` | `meta_invariants.rs`, `session_atomicity.rs`, `barrier_fault_injection.rs`, `cross_process_put.rs`, `cli_sigkill.rs`, `forge-store/objectstore/conformance.rs`, `docs/RECOVERY.md`, `docs/OBJECTSTORE.md` |
| I5, I7, I8 | `forge-store/meta.rs`, `forge-api/workspace.rs`, `refs.rs` | `api_contract.rs`, `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `fsck_concurrent_fork.rs`, `multi_mount_shape.rs`, `fuzz/ref_name` |
| I9 | `forge-api/workspace.rs` | `api_contract.rs`, `e2e_concurrent.rs`, `multi_mount_shape.rs` |
| I18 | `forge-api/workspace.rs`, `forge-api/gc.rs`, `forge-store/meta.rs` | `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `gc_and_abandon.rs`, `docs/GC.md` |
| I19, I20, I21 | `forge-api/workspace.rs` (`mount`, `session_mount_tree`, `check_observations`, `checkin`), `forge-store/meta.rs` (`mounts.base_oid`, `insert_mount`, `cas_ref_session`, `MIGRATE_2_TO_3`), `forge-api/gc.rs`, `fsck.rs` | `multi_mount_shape.rs`, `multi_mount_concurrent.rs`, `cli_mount_pin.rs`, `schema_migration_fixtures.rs`, `testdata/schema/v2_pre_mount_pin.sql`, `session_atomicity.rs`, `docs/GC.md` |
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

## Shape gaps that remain

I1-I18 were all safety properties: each one named something that must never
happen. None required anything to eventually happen, and none required an
operation to act on *everything* in front of it. Both P0s that reached
production here sat in exactly those two blind spots -- #233 because nothing
said where a read resolves FROM, #326 because nothing said checkin must publish
or explicitly refuse everything staged. I19-I21 close the first blind spot and
the reachable part of the second. What the audit on `docs/invariant-shape-audit`
found and this change does **not** fix, stated so it is not mistaken for
covered:

* **Checkin still reports `Noop` while another mount holds staged work.** I19
  makes that work publishable -- `checkin --mount <path>` folds it onto that
  mount's own base and CASes that mount's ref -- but `Noop` on `/` still means
  "this mount stages nothing", not "the session stages nothing anywhere", so
  `checkin` and `abandon` can still disagree about whether a session holds work.
  That is #326's remaining half and belongs to `fix/checkin-noop-drops-staged-work`.
  `multi_mount_shape.rs::checkin_reports_noop_while_another_mount_holds_unpublished_work`
  pins it.
* **The observation epoch is per-session while the overlay epoch is per-mount.**
  `Meta::cas_ref_session` deletes observations for the whole namespace while
  deleting overlay for the published mount alone, so a foreign-mount read stops
  constraining the session at the first checkin of any *other* mount. Either the
  epoch is per-session -- in which case say so, and clear the overlay with it --
  or it is per-mount, in which case the `DELETE FROM observations` must be scoped
  the way `DELETE FROM overlay` is.
  `multi_mount_shape.rs::a_checkin_of_one_mount_forgets_every_other_mounts_observations`
  pins it.
* **`Meta::commit_seal` performs no compare-and-swap against the ref it seals.**
  The snapshot is internally consistent, but `seal main v1` can publish a tag
  naming a commit `main` no longer held when the tag became visible.

The `fuzz/` targets cover the same boundaries with untrusted bytes: typed
decoders (`object_decode`), the capability parser (`cap_token`), daemon framing
and dispatch (`protocol_frame`), the tree-name and ref-name grammars
(`tree_name`, `ref_name`), and the tar export/import round trip
(`tar_roundtrip`). CI compiles every target and smokes each for 60 seconds.
