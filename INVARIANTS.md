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
| I22 | `Noop` is the one checkin outcome that may never be said over work that exists. A checkin that folds its mount and finds nothing to publish refuses -- exit 1 -- and names every other mount holding staged entries, instead of answering "there was nothing to do" for a session that plainly has something to do. `updated` and `forked` are progress and may legitimately leave another mount staged: under I19 a session holds a pin per writable mount and publishes them one `checkin --mount` at a time, so refusing those too would deny the very escape the refusal advises (I21). I18 forbids the failure path that destroys staged work; I22 forbids the success path that denies it exists, so `checkin` and `abandon` can never disagree about whether a session still holds work. |
| I23 | Collection unlinks only bytes nothing can reach. A sweep reads the root set and unlinks inside the one catalog write transaction every root publication commits under, so mark and publish cannot interleave; it re-reads each candidate's age under the object plane's exclusive lock, the same lock a deduplicating put refreshes an age under, so content addressing cannot hide a live object behind an old timestamp; it spares everything any surviving object can reach, because `fsck --full` roots object files and not merely catalog roots; and every catalog row naming a swept object is removed in the same transaction. Collection is sound while no single put-to-publish interval exceeds the grace floor, which is why the floor has a hard minimum and no session lease is required: a pin is a catalog row, so it is a root. |

A composed Unicode name and its canonically equivalent decomposed spelling are distinct ForgeFS entries if their UTF-8 byte strings differ. Likewise `Foo` and `foo` are distinct. Export adapters must detect target-filesystem collisions and fail rather than silently normalize, fold, or overwrite names. `export_tar` refuses a directory whose sibling names collide under case folding or Unicode canonical equivalence and names both spellings with their bytes; `ExportOptions::allow_name_collisions` (`forge export --allow-name-collisions`) is the deliberate per-call opt-out for a case-sensitive destination, never a default and never inferred from the exporting host.

## Executable evidence

The public cross-cutting seam is table-tested in
[`api_contract.rs`](crates/forge-api/tests/api_contract.rs). Mechanism-specific
proofs stay separate so a real race, process, crash, or filesystem boundary is
not diluted into a mock:

| Invariants | Production owner | Primary evidence |
|---|---|---|
| I1, I2, I10, I17 | `forge-core`, `forge-store/graph.rs`, `forge-store/meta.rs`, `forge-api/import.rs`, `integration.rs`, `repository.rs` | `golden_object_ids.rs`, `adversarial_canonical.rs`, `provenance.rs`, `checkin_contribution.rs`, `typed_graph.rs`, `api_contract.rs`, `bootstrap_contract.rs`, `schema_migrations.rs`, `schema_migration_fixtures.rs`, `schema_migration_objects.rs`, `testdata/schema/README.md`, `property_canonical.rs`, `large_blob_memory.rs`, `fuzz/tree_name` |
| I3, I4, I6 | `forge-store`, `repository.rs` | `meta_invariants.rs`, `group_commit.rs`, `session_atomicity.rs`, `barrier_fault_injection.rs`, `cross_process_put.rs`, `cli_sigkill.rs`, `forge-store/objectstore/conformance.rs`, `docs/RECOVERY.md`, `docs/OBJECTSTORE.md` |
| I5, I7, I8 | `forge-store/meta.rs`, `forge-api/workspace.rs`, `refs.rs` | `api_contract.rs`, `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `fsck_concurrent_fork.rs`, `multi_mount_shape.rs`, `fuzz/ref_name` |
| I9 | `forge-api/workspace.rs` | `api_contract.rs`, `e2e_concurrent.rs`, `multi_mount_shape.rs` |
| I18 | `forge-api/workspace.rs`, `forge-api/gc.rs`, `forge-store/meta.rs` | `pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `gc_and_abandon.rs`, `docs/GC.md` |
| I19, I20, I21 | `forge-api/workspace.rs` (`mount`, `session_mount_tree`, `check_observations`, `checkin`), `forge-store/meta.rs` (`mounts.base_oid`, `insert_mount`, `cas_ref_session`, `MIGRATE_2_TO_3`), `forge-api/gc.rs`, `fsck.rs` | `multi_mount_shape.rs`, `multi_mount_concurrent.rs`, `cli_mount_pin.rs`, `schema_migration_fixtures.rs`, `testdata/schema/v2_pre_mount_pin.sql`, `session_atomicity.rs`, `docs/GC.md` |
| I22 | `forge-api/workspace.rs` (`checkin`), `forge-store/meta.rs` (`overlay_mounts_outside`), `forge-cli/main.rs` | `checkin_staged_work.rs`, `cli_checkin_staged_work.rs`, `multi_mount_shape.rs`, `gc_and_abandon.rs`, `CLI_ABI.md` |
| I23 | `forge-api/gc.rs` (`gc_collect`, `schedule_catalog_roots`), `forge-store/meta.rs` (`gc_sweep`, `GcCatalogRoots`), `forge-store/blob.rs` (`refresh_dedup_mtime`) | `gc_collect_concurrent.rs`, `gc_collect.rs`, `gc_and_abandon.rs`, `cache_trust.rs`, `docs/GC.md` |
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
or explicitly refuse everything staged. I19-I21 close the first blind spot;
I22 closes the second, scoped to the `noop` outcome, because `updated` and
`forked` are progress and may legitimately leave another mount staged. What the
audit on `docs/invariant-shape-audit` found and these changes do **not** fix,
stated so it is not mistaken for covered:

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
* **`serve` is outside the contract entirely (#332).** No invariant names it,
  `CLI_ABI.md` does not describe it, and `scripts/cli-abi-conformance.sh`
  exercises the CLI binary alone -- so the daemon's exit codes, error mapping
  and argument surface are unspecified and untested. The specific superset
  #332 reported is closed: `ns.checkin` takes a caller-chosen `mount`, and the
  CLI can now express it too (`checkin --mount`). The contract gap is not.
  Either document the daemon as its own ABI with its own conformance rows, or
  constrain `dispatch` to what the CLI can express -- before anyone builds
  against `serve` and today's undocumented behaviour becomes tomorrow's
  compatibility obligation.

### Which operations an invariant actually constrains

The audit that produced I19-I22 worked by asking, of every verb, *which rule
would catch this being wrong*. That question is worth keeping answerable, since
this project merges on "a PR that cannot name an invariant does not merge" and
an operation no rule names is a bug class nothing prevents. As of I23:

| Operation | Constrained by |
|---|---|
| `session open` | I8 (pins a base), I19 (the root mount is pinned with it) |
| `mount` | I19 (a read-write mount carries its own pin), I20 (it must be publishable) |
| `read`, `ls` | I8, I9 (every read is recorded), I19 (resolved against the mount's pin) |
| `checkin` | I5, I8, I9, I10, I18, I19, I21, I22 |
| `abandon` | I18 (a fork stays a root until deliberately retired) |
| `gc` | I23 (collection unlinks only unreachable bytes) |
| `merge` | I11 (overlap is a Conflict object), I12 (order comes from the DAG) |
| `seal`, `verify`, `fsck` | I6, I10, I15 |
| `import`, `export` | I1, I2, I16 |
| `grant` | I13, I14 |
| **`write`** | **nothing** -- staging is constrained only downstream, by what I22 makes checkin say about it |
| **`branch`** | **nothing.** `Forge::branch` calls `Meta::insert_ref`, not a CAS, so I5 -- which governs how refs *move* -- does not cover ref *creation* at all |
| **`serve`** | **nothing** (#332, above) |
| **`inbox`, `landmark`, `init`, `refs`, `log`, `show`, `stats`** | **nothing** |

The unconstrained rows are not all equally alarming -- `log` and `show` are
read-only projections -- but `write`, `branch` and `serve` each move or create
durable state, and each is a place where the next unstated-invariant defect can
sit. `mount` and `gc` were on this list until I19/I20 and I23; both had live
defects (#327, #328, #330, #333, #12) waiting in exactly that silence.

### Shape of the set

The audit's original headline -- *all eighteen rules are safety properties, none
is liveness* -- was true of I1-I18 and is no longer true of the set as a whole.
I21 is a genuine liveness rule: it requires that a session with write authority
can always *reach* a terminal state, not merely that it never reaches a wrong
one. Totality rules -- the ones that require an operation to act on everything
in front of it rather than on some of it -- are I18, I22, and the I16 export
collision paragraph. Everything else remains safety.

The `fuzz/` targets cover the same boundaries with untrusted bytes: typed
decoders (`object_decode`), the capability parser (`cap_token`), daemon framing
and dispatch (`protocol_frame`), the tree-name and ref-name grammars
(`tree_name`, `ref_name`), and the tar export/import round trip
(`tar_roundtrip`). CI compiles every target and smokes each for 60 seconds.
