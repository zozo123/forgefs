//! I17: the metadata schema version is independent of the repository VERSION
//! that gates immutable decoding. A forward migration must carry every mutable
//! row across unchanged, must never touch an ObjectId, and must fail closed on
//! any version -- or any shape -- this binary cannot honour.
//!
//! Historical fixtures live in `testdata/schema/`. Only one metadata schema has
//! ever shipped (`CURRENT_SCHEMA_VERSION = 1`), so the single migration the
//! code implements is `0 -> 1`, where version 0 is the pre-versioning catalog
//! that `schema_version()` itself defines as "no `schema_migrations` ledger".

use forge_store::{Meta, CURRENT_SCHEMA_VERSION};
use forge_types::Error;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PRE_VERSIONING: &str = include_str!("../../../testdata/schema/v0_pre_versioning.sql");
const SHAPE_DRIFT: &str = include_str!("../../../testdata/schema/v0_shape_drift.sql");

/// Every retired schema version needs frozen bytes to migrate from. The guard
/// test below fails the moment `CURRENT_SCHEMA_VERSION` is bumped without one,
/// which is the intended entry point to `testdata/schema/README.md`.
const RETIRED_SCHEMA_FIXTURES: &[(i64, &str)] = &[(0, PRE_VERSIONING)];

/// Relations whose rows must survive a migration untouched. `cap_root` is
/// excluded on purpose: a writable open scrubs a legacy root HMAC key (I14),
/// which is asserted separately.
const PRESERVED_TABLES: &[&str] = &[
    "refs",
    "reflog",
    "namespaces",
    "observations",
    "mounts",
    "overlay",
    "seals",
    "landmarks",
    "object_intro",
];

fn catalog_from(sql: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("meta.sqlite");
    let conn = Connection::open(&path).expect("create fixture catalog");
    conn.execute_batch(sql).expect("apply fixture sql");
    drop(conn);
    (dir, path)
}

fn open_fixture(path: &Path) -> Connection {
    Connection::open(path).expect("reopen catalog")
}

/// A shape-independent dump of one relation: `quote()` renders blobs as
/// `X'..'` literals, so an ObjectId that changed by a single byte changes the
/// dump. Rows are ordered by their full text so row order cannot mask a loss.
fn dump_table(conn: &Connection, table: &str) -> Vec<String> {
    let columns: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .expect("table_info")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("table_info rows")
        .collect::<Result<_, _>>()
        .expect("table_info column");
    assert!(!columns.is_empty(), "table {table} is missing");
    let projection = columns
        .iter()
        .map(|column| format!("quote(\"{column}\")"))
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    let sql = format!("SELECT {projection} AS row FROM \"{table}\" ORDER BY row");
    conn.prepare(&sql)
        .expect("dump statement")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("dump rows")
        .collect::<Result<_, _>>()
        .expect("dump row")
}

fn dump_all(conn: &Connection) -> Vec<(String, Vec<String>)> {
    PRESERVED_TABLES
        .iter()
        .map(|table| ((*table).to_string(), dump_table(conn, table)))
        .collect()
}

fn ledger(conn: &Connection) -> Vec<i64> {
    if !table_exists(conn, "schema_migrations") {
        return Vec::new();
    }
    conn.prepare("SELECT version FROM schema_migrations ORDER BY version")
        .expect("ledger statement")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("ledger rows")
        .collect::<Result<_, _>>()
        .expect("ledger row")
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .expect("sqlite_master lookup");
    found != 0
}

fn hex(byte: u8) -> String {
    std::iter::repeat_n(byte, 32)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn pre_versioning_catalog_carries_every_row_forward() {
    let (_dir, path) = catalog_from(PRE_VERSIONING);
    let before = open_fixture(&path);
    assert!(
        !table_exists(&before, "schema_migrations"),
        "the fixture must start at schema version 0"
    );
    let before_rows = dump_all(&before);
    drop(before);

    let meta = Meta::open(&path).expect("a pre-versioning catalog must migrate forward");
    let refs = meta.list_refs().expect("migrated refs must be readable");
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["main", "snap/one"]);
    assert_eq!(refs[0].oid.hex(), hex(0xa1));
    assert_eq!(refs[1].oid.hex(), hex(0xb2));
    assert!(refs[0].protected, "protected must survive migration");
    assert!(refs[1].sealed, "sealed must survive migration");
    drop(meta);

    let after = open_fixture(&path);
    assert_eq!(ledger(&after), vec![CURRENT_SCHEMA_VERSION]);
    assert_eq!(
        dump_all(&after),
        before_rows,
        "migration must not rewrite, drop, or reorder any mutable row"
    );
}

/// I14: a pre-versioning catalog may still carry a root HMAC key. Migration is
/// the moment it is scrubbed, and the trusted seal key must not move with it.
#[test]
fn migration_scrubs_a_legacy_root_secret_without_touching_the_seal_key() {
    let (_dir, path) = catalog_from(PRE_VERSIONING);
    let meta = Meta::open(&path).expect("migrate");
    assert_eq!(meta.get_seal_pub().expect("seal key"), vec![0x5e; 32]);
    drop(meta);

    let after = open_fixture(&path);
    let hmac: Vec<u8> = after
        .query_row("SELECT hmac_key FROM cap_root WHERE id=1", [], |row| {
            row.get(0)
        })
        .expect("cap_root row survives migration");
    assert!(hmac.is_empty(), "legacy root secret must be scrubbed");
}

/// A migration that cannot reach the target shape must refuse. `CREATE TABLE
/// IF NOT EXISTS` is a no-op against an existing relation with an older column
/// list, so recording the new version would make the catalog lie about itself
/// forever: every later open takes the "already current" path and the first
/// query fails as a raw SQLite error instead of a migration diagnostic.
#[test]
fn migration_that_cannot_reach_the_current_shape_fails_closed() {
    let (_dir, path) = catalog_from(SHAPE_DRIFT);
    let error = Meta::open(&path).err().unwrap_or_else(|| {
        let conn = open_fixture(&path);
        let refs = dump_table(&conn, "refs");
        panic!(
            "migration recorded schema version {:?} for a catalog whose `refs` \
             relation never gained the `sealed` column: the ledger now claims a \
             version the catalog never reached, every later open takes the \
             already-current path, and ref reads fail as raw SQLite \
             \"no such column\" errors instead of a migration diagnostic (refs \
             still {refs:?})",
            ledger(&conn)
        )
    });
    let text = error.to_string();
    assert!(matches!(error, Error::Invalid(_)), "unexpected: {text}");
    assert!(
        text.contains("refs") && text.contains("sealed"),
        "the diagnostic must name the relation and the missing column: {text}"
    );

    let after = open_fixture(&path);
    assert!(
        ledger(&after).is_empty(),
        "a refused migration must not claim a schema version"
    );
    assert!(
        !table_exists(&after, "namespaces"),
        "a refused migration must roll back every relation it created"
    );
    assert_eq!(
        dump_table(&after, "refs").len(),
        1,
        "a refused migration must leave the operator's rows intact"
    );
    drop(after);

    assert!(
        Meta::open(&path).is_err(),
        "a refused migration must stay refused on the next open"
    );
}

/// Fail closed on the future: a catalog from a newer binary is never opened
/// writable, never opened read-only, and never migrated backwards.
#[test]
fn a_newer_schema_version_is_refused_by_every_normal_open() {
    let (dir, path) = catalog_from(PRE_VERSIONING);
    drop(Meta::open(&path).expect("migrate to current"));
    let conn = open_fixture(&path);
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, 0)",
        [CURRENT_SCHEMA_VERSION + 1],
    )
    .expect("stamp a future version");
    let rows = dump_all(&conn);
    drop(conn);

    for error in [
        Meta::open(&path).err().expect("writable open must refuse"),
        Meta::open_read_only(&path)
            .err()
            .expect("read-only open must refuse"),
    ] {
        let text = error.to_string();
        assert!(matches!(error, Error::Invalid(_)), "unexpected: {text}");
        assert!(
            text.contains("newer than supported"),
            "diagnostic must name the incompatibility: {text}"
        );
    }

    // Fsck is the one path allowed to inspect an incompatible ledger, so an
    // operator can diagnose instead of being locked out with no report.
    drop(Meta::open_read_only_for_fsck(&path).expect("fsck must still inspect"));

    let conn = open_fixture(&path);
    assert_eq!(
        ledger(&conn),
        vec![CURRENT_SCHEMA_VERSION, CURRENT_SCHEMA_VERSION + 1],
        "a refused open must not rewrite the ledger"
    );
    assert_eq!(dump_all(&conn), rows, "a refused open must not touch rows");
    drop(conn);
    drop(dir);
}

/// Migration is a write, so a read-only open must say so rather than fail
/// later inside a query against a column the on-disk schema does not have.
#[test]
fn a_read_only_open_refuses_to_migrate_a_pre_versioning_catalog() {
    let (_dir, path) = catalog_from(PRE_VERSIONING);
    let error = Meta::open_read_only(&path)
        .err()
        .expect("read-only cannot migrate");
    let text = error.to_string();
    assert!(matches!(error, Error::Invalid(_)), "unexpected: {text}");
    assert!(
        text.contains("needs migration"),
        "diagnostic must name migration as the blocker: {text}"
    );

    let after = open_fixture(&path);
    assert!(
        !table_exists(&after, "schema_migrations"),
        "a read-only open must not migrate the catalog it refused"
    );
}

/// The harness that catches the first real migration. Bumping
/// `CURRENT_SCHEMA_VERSION` without freezing a catalog produced by the binary
/// that shipped the retired version fails here, before the untested migration
/// can reach a repository.
#[test]
fn every_retired_schema_version_has_a_migration_fixture() {
    let covered: Vec<i64> = RETIRED_SCHEMA_FIXTURES
        .iter()
        .map(|(version, _)| *version)
        .collect();
    let expected: Vec<i64> = (0..CURRENT_SCHEMA_VERSION).collect();
    assert_eq!(
        covered, expected,
        "every schema version below {CURRENT_SCHEMA_VERSION} needs a frozen \
         catalog fixture in testdata/schema/ and a migration proof here; \
         see testdata/schema/README.md"
    );
    for (version, sql) in RETIRED_SCHEMA_FIXTURES {
        let (_dir, path) = catalog_from(sql);
        drop(Meta::open(&path).unwrap_or_else(|error| {
            panic!("schema version {version} fixture must migrate forward: {error}")
        }));
        let conn = open_fixture(&path);
        assert_eq!(ledger(&conn), vec![CURRENT_SCHEMA_VERSION]);
    }
}
