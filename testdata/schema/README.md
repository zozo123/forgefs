# Metadata schema fixtures

`testdata/canonical/` freezes immutable object bytes. This directory freezes
the other axis: **mutable SQLite catalogs at retired metadata schema
versions**. The two are independent by I17 — a metadata migration never
rewrites objects or ObjectIds, and a repository VERSION transition is never a
reason to rewrite a catalog fixture.

## What exists today

ForgeFS has shipped exactly one metadata schema: `CURRENT_SCHEMA_VERSION = 1`.
There is therefore no released version-1-to-2 history to migrate from, and
nothing here pretends otherwise.

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

Evidence: `crates/forge-store/tests/schema_migration_fixtures.rs` and
`crates/forge-api/tests/schema_migration_objects.rs`.

## Procedure for the first real migration (adding schema version 2)

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
   as an explicit `1 -> 2` arm. Migrations run inside one `IMMEDIATE`
   transaction and must leave immutable objects untouched.

3. **Register the fixture** in the `RETIRED_SCHEMA_FIXTURES` table of
   `crates/forge-store/tests/schema_migration_fixtures.rs` and extend the
   preservation assertions with whatever the new step is supposed to change.
   A migration that transforms rows must state the expected transformation as
   an assertion, not as prose.

4. **Keep the fail-closed proofs.** A catalog at version 3 must still be
   refused by every writable and read-only open, and a migration that cannot
   reach the target shape must refuse rather than record the new version.

5. Never regenerate `testdata/canonical/*` as part of a schema migration.
