# Metadata schema fixtures

`testdata/canonical/` freezes immutable object bytes. This directory freezes
the other axis: **mutable SQLite catalogs at retired metadata schema
versions**. The two are independent by I17 — a metadata migration never
rewrites objects or ObjectIds, and a repository VERSION transition is never a
reason to rewrite a catalog fixture.

## What exists today

`CURRENT_SCHEMA_VERSION` is **3**. Versions 1 and 2 shipped; version 0 is the
pre-versioning state defined below. Every retired version has frozen bytes here
and a migration proof in `schema_migration_fixtures.rs`.

Version **0** is not an invented release. `schema_version()` in
`crates/forge-store/src/meta.rs` defines 0 as *a catalog with no
`schema_migrations` ledger*, and `migrate` carries an explicit `0 -> 1` step
for that state. `v0_pre_versioning.sql` is that state, populated with rows in
every relation, so the one migration the code actually implements is proved
end to end instead of assumed.

| Fixture | Meaning |
|---|---|
| `v0_pre_versioning.sql` | A populated pre-versioning catalog. Migrating it must preserve every row and every ObjectId byte-for-byte, and record ledger `[1]`. |
| `v0_shape_drift.sql` | Synthetic, never shipped: a version-0 catalog whose `refs` relation lacks a column the current schema requires. `CREATE TABLE IF NOT EXISTS` cannot fix such a relation, so the migration must fail closed instead of recording a version it did not reach. |
| `v1_pre_observation_kind.sql` | The version-1 shape, before `observations` grew its `kind` discriminant. Migrating it must carry every existing observation forward as the blob observation it was. |
| `v2_pre_mount_pin.sql` | The version-2 shape, captured with the v0.2.1 binary: before `mounts` grew `base_oid`, i.e. before every read-write mount had its own pinned base (I19). It carries two live sessions with staged overlay and recorded observations, a second read-write mount on a foreign ref, a read-only mount on that same ref, and a read-write raw-`oid:` mount, because those are the four states `MIGRATE_2_TO_3` has to decide about. |

Evidence: `crates/forge-store/tests/schema_migration_fixtures.rs` and
`crates/forge-api/tests/schema_migration_objects.rs`.

## Two migrations have gone through this procedure

`MIGRATE_1_TO_2` reshaped `observations`. `MIGRATE_2_TO_3` added
`mounts.base_oid` and backfilled it per mount from the ref that mount names; its
reasoning -- and why neither "backfill everything from the session pin" nor
"leave it NULL and resolve live" was acceptable -- is written out above the
constant in `crates/forge-store/src/meta.rs`. Note the one non-obvious
constraint it exposed: schema version 0 means *either* a fresh catalog *or* a
pre-versioning one, and the second can already carry relations at any earlier
shape, including the current one when a catalog has lost its ledger. A migration
step must therefore be safe to re-apply. `MIGRATE_1_TO_2` is, because it rebuilds
its relation; an unconditional `ALTER TABLE ... ADD COLUMN` is not, so
`migrate_2_to_3` probes for the column first and guards every backfill statement
on `base_oid IS NULL`.

## Procedure for the next migration

`every_retired_schema_version_has_a_migration_fixture` fails the moment
`CURRENT_SCHEMA_VERSION` is bumped without a fixture for the version being
retired. That failure is the entry point to this checklist:

1. **Capture, do not synthesize.** Create a repository with the last binary
   that shipped the outgoing version and dump its catalog:

   ```sh
   forge init /tmp/v1repo && forge -C /tmp/v1repo import ... # exercise refs,
   # reflog, namespaces, mounts, overlay, seals
   sqlite3 /tmp/v1repo/.forge/meta.sqlite .dump > testdata/schema/v1_catalog.sql
   ```

   Trim volatile timestamps to fixed values so the fixture is deterministic,
   and keep it small. Never hand-write a shape a release did not produce; the
   only hand-written fixtures here are named as synthetic.

2. **Add the migration step** in `migrate` (`crates/forge-store/src/meta.rs`)
   as an explicit `N -> N+1` arm, safe to re-apply. Migrations run inside one `IMMEDIATE`
   transaction and must leave immutable objects untouched.

3. **Register the fixture** in the `RETIRED_SCHEMA_FIXTURES` table of
   `crates/forge-store/tests/schema_migration_fixtures.rs` and extend the
   preservation assertions with whatever the new step is supposed to change.
   A migration that transforms rows must state the expected transformation as
   an assertion, not as prose.

4. **Keep the fail-closed proofs.** A catalog one version past
   `CURRENT_SCHEMA_VERSION` must still be refused by every writable and read-only open, and a migration that cannot
   reach the target shape must refuse rather than record the new version.

5. Never regenerate `testdata/canonical/*` as part of a schema migration.
