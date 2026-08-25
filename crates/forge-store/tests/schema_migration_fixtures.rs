//! I17: the metadata schema version is independent of the repository VERSION
//! that gates immutable decoding. A forward migration must carry every mutable
//! row across unchanged, must never touch an ObjectId, and must fail closed on
//! any version -- or any shape -- this binary cannot honour.
//!
//! Historical fixtures live in `testdata/schema/`. Version 0 is the
//! pre-versioning catalog that `schema_version()` itself defines as "no
//! `schema_migrations` ledger"; 1 and 2 are shipped shapes.

use forge_store::{Meta, CURRENT_SCHEMA_VERSION};
use forge_types::Error;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PRE_VERSIONING: &str = include_str!("../../../testdata/schema/v0_pre_versioning.sql");
const SHAPE_DRIFT: &str = include_str!("../../../testdata/schema/v0_shape_drift.sql");
const V1_PRE_OBSERVATION_KIND: &str =
    include_str!("../../../testdata/schema/v1_pre_observation_kind.sql");
const V2_PRE_MOUNT_PIN: &str = include_str!("../../../testdata/schema/v2_pre_mount_pin.sql");

/// Every retired schema version needs frozen bytes to migrate from. The guard
/// test below fails the moment `CURRENT_SCHEMA_VERSION` is bumped without one,
/// which is the intended entry point to `testdata/schema/README.md`.
const RETIRED_SCHEMA_FIXTURES: &[(i64, &str)] = &[
    (0, PRE_VERSIONING),
    (1, V1_PRE_OBSERVATION_KIND),
    (2, V2_PRE_MOUNT_PIN),
];

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

/// Every version stepped through is recorded, so a half-applied catalog is
/// distinguishable from one that reached the current shape.
fn expected_ledger() -> Vec<i64> {
    (1..=CURRENT_SCHEMA_VERSION).collect()
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

fn ref_oid(conn: &Connection, name: &str) -> forge_types::ObjectId {
    let bytes: Vec<u8> = conn
        .query_row("SELECT oid FROM refs WHERE name=?1", [name], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("ref {name}: {error}"));
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes);
    forge_types::ObjectId(id)
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
    assert_eq!(ledger(&after), expected_ledger());
    // Two tables are deliberately reshaped on the way to the current version,
    // so asserting byte-equality across them would be asserting the migrations
    // did nothing: v2 gives `observations` a `kind` discriminant, and v3 gives
    // `mounts` a `base_oid` and backfills it (I19). Every OTHER table must be
    // untouched, and both reshaped relations are checked as transformations --
    // here for `observations`, and in
    // `v2_catalog_gives_every_read_write_mount_its_own_pin` for `mounts`.
    let reshaped = |rows: &[(String, Vec<String>)]| -> Vec<(String, Vec<String>)> {
        rows.iter()
            .filter(|(table, _)| table != "observations" && table != "mounts")
            .cloned()
            .collect()
    };
    assert_eq!(
        reshaped(&dump_all(&after)),
        reshaped(&before_rows),
        "migration must not rewrite, drop, or reorder any mutable row"
    );
    // The one `mounts` row the pre-versioning fixture carries is read-write on
    // spec `main`, which is also its namespace's `live_ref`, so I19 backfills it
    // from `namespaces.pinned_oid` -- the session's own base, preserved exactly.
    assert_eq!(
        dump_table(&after, "mounts"),
        vec![format!(
            "'ns-fixture-0001'|'/src'|'main'|'rw'|X'{}'",
            hex(0xa1).to_uppercase()
        )],
        "a read-write mount on the session's own live ref must be pinned to the session pin"
    );

    // Every pre-existing observation is carried forward as the blob
    // observation it was: same key, same oid, tagged `blob`.
    let obs_before = &before_rows
        .iter()
        .find(|(t, _)| t == "observations")
        .expect("fixture has observations")
        .1;
    let obs_after = dump_all(&after)
        .into_iter()
        .find(|(t, _)| t == "observations")
        .expect("migrated catalog has observations")
        .1;
    assert_eq!(
        obs_after.len(),
        obs_before.len(),
        "migration must not drop or duplicate an observation"
    );
    for (before_row, after_row) in obs_before.iter().zip(&obs_after) {
        let (key, oid) = before_row
            .rsplit_once('|')
            .expect("observation row is key|oid");
        assert_eq!(
            after_row,
            &format!("{key}|'blob'|{oid}"),
            "an existing observation must carry forward as a blob observation"
        );
    }
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

/// I19/I17: v3 gives every read-write mount its own pinned base. The migration
/// has to decide what an existing read-write row means, and the two obvious
/// answers are both wrong -- backfilling every one from `namespaces.pinned_oid`
/// freezes the defect that a mount of another ref serves the session's base,
/// and leaving them NULL to resolve live reintroduces #233 for every session
/// open across the upgrade. This asserts the decision actually taken, per mount,
/// from the ref that mount names.
#[test]
fn v2_catalog_gives_every_read_write_mount_its_own_pin() {
    let (_dir, path) = catalog_from(V2_PRE_MOUNT_PIN);
    let before = open_fixture(&path);
    assert_eq!(ledger(&before), vec![1, 2], "the fixture must start at v2");
    let before_rows = dump_all(&before);
    let base = ref_oid(&before, "base");
    let other = ref_oid(&before, "other");
    assert_ne!(base, other, "the fixture's two refs must have diverged");
    drop(before);

    let meta = Meta::open(&path).expect("a v2 catalog must migrate forward");
    let sessions: Vec<String> = meta
        .list_namespaces()
        .expect("namespaces")
        .into_iter()
        .map(|ns| ns.id)
        .collect();
    assert_eq!(sessions.len(), 2, "the fixture has two live sessions");

    // Session A is the one holding several read-write mounts; session B is the
    // ordinary single-mount shape. Identify them by their mount tables, not by
    // ULID order, so the fixture can be regenerated without editing this test.
    let mut multi = None;
    let mut single = None;
    for id in &sessions {
        let mounts = meta.list_mounts(id).expect("mounts");
        if mounts.len() > 2 {
            multi = Some((id.clone(), mounts));
        } else {
            single = Some((id.clone(), mounts));
        }
    }
    let (multi_id, multi_mounts) = multi.expect("session A");
    let (single_id, single_mounts) = single.expect("session B");

    let pin_of = |mounts: &[forge_store::MountRow], path: &str| {
        mounts
            .iter()
            .find(|m| m.path == path)
            .unwrap_or_else(|| panic!("mount {path}"))
            .clone()
    };

    // The session's OWN ref keeps the session pin, exactly: #233 stays fixed
    // for the ordinary session, and the value is preserved rather than
    // re-derived.
    let own = pin_of(&single_mounts, "/");
    let pin = meta
        .get_namespace(&single_id)
        .expect("namespace")
        .pinned_oid
        .expect("pin");
    assert_eq!(
        own.base_oid,
        Some(pin),
        "a mount on the session's own live ref must be pinned to the session pin"
    );

    // A read-write mount of ANOTHER ref is pinned to THAT ref, not to the
    // session base it used to be served from. This is bug A, fixed on upgrade.
    let root = pin_of(&multi_mounts, "/");
    let foreign = pin_of(&multi_mounts, "/other");
    assert_eq!(root.base_oid, Some(base), "/ must be pinned to ref:base");
    assert_eq!(
        foreign.base_oid,
        Some(other),
        "a read-write mount of ref:other must be pinned to ref:other, not to the session base"
    );

    // I19: publishing is what moves a pin, and this migration publishes
    // nothing, so session A's own base is exactly where it was.
    assert_eq!(
        meta.get_namespace(&multi_id).expect("namespace").pinned_oid,
        Some(base),
        "the migration must not move a session pin"
    );

    // A read-only mount takes no pin: resolving live is what it is for (I9).
    assert_eq!(pin_of(&multi_mounts, "/dep").base_oid, None);
    assert_eq!(pin_of(&multi_mounts, "/main").base_oid, None);

    // A read-write raw `oid:` mount is demoted, because v3 refuses to create
    // one: an immutable spec has no ref to advance, so it was a write path with
    // no publish path, and `fsck` reported the row as MOUNT_RW_OID corruption.
    let snap = pin_of(&multi_mounts, "/snap");
    assert!(snap.spec.starts_with("oid:"), "{snap:?}");
    assert_eq!(snap.mode, "ro", "a read-write oid mount must be demoted");
    assert_eq!(snap.base_oid, None);

    // I18: the upgrade publishes nothing and discards nothing. Every staged
    // overlay entry and every observation is still there, byte for byte.
    drop(meta);
    let after = open_fixture(&path);
    assert_eq!(ledger(&after), expected_ledger());
    let keep = |rows: &[(String, Vec<String>)]| -> Vec<(String, Vec<String>)> {
        rows.iter()
            .filter(|(table, _)| table != "mounts")
            .cloned()
            .collect()
    };
    assert_eq!(
        keep(&dump_all(&after)),
        keep(&before_rows),
        "the v3 migration must touch nothing but the mounts relation"
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
        {
            let mut expected = expected_ledger();
            expected.push(CURRENT_SCHEMA_VERSION + 1);
            expected
        },
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
        assert_eq!(ledger(&conn), expected_ledger());
    }
}
