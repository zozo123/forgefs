use forge_store::{Meta, Observed, CURRENT_SCHEMA_VERSION};
use forge_types::{Error, ObjectId};
use rusqlite::Connection;
use tempfile::tempdir;

fn schema_ledger(path: &std::path::Path) -> Vec<i64> {
    Connection::open(path)
        .unwrap()
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// Metadata schema versioning is deliberately separate from immutable object format versioning.
#[test]
fn fresh_metadata_records_every_supported_version() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let meta = Meta::open(&path).unwrap();
    drop(meta);

    // The ledger must stay contiguous from 1, which is what `fsck --full`
    // checks, so a fresh catalog records every version it already embodies.
    assert_eq!(
        schema_ledger(&path),
        (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>()
    );
}

// I9: a v1 catalog opened by v2 code must carry its recorded reads forward as
// the blob observations they were, not lose them and not be rebuilt as a fresh
// repository.
#[test]
fn migrating_v1_observations_preserves_them_as_blob_reads() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    drop(Meta::open(&path).unwrap());

    // Put a real catalog back into its v1 shape: the observations table before
    // it could say anything but "a blob was here".
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE observations;
         CREATE TABLE observations (
           ns_id TEXT NOT NULL,
           mount TEXT NOT NULL,
           path  TEXT NOT NULL,
           oid   BLOB NOT NULL CHECK(length(oid)=32),
           PRIMARY KEY (ns_id, mount, path)
         );
         DELETE FROM schema_migrations WHERE version > 1;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO observations (ns_id, mount, path, oid) VALUES ('ns','/','a.txt',?1)",
        [vec![7u8; 32]],
    )
    .unwrap();
    drop(conn);

    let meta = Meta::open(&path).unwrap();
    let rows = meta.observations("ns").unwrap();
    assert_eq!(rows.len(), 1, "migration dropped the recorded read");
    assert_eq!(rows[0].path, "a.txt");
    assert_eq!(rows[0].seen, Observed::Blob(ObjectId([7u8; 32])));
    drop(meta);

    assert_eq!(
        schema_ledger(&path),
        (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>()
    );
}

#[test]
fn newer_metadata_schema_fails_closed() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let meta = Meta::open(&path).unwrap();
    drop(meta);

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, 0)",
        [CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();
    drop(conn);

    let err = Meta::open(&path)
        .err()
        .expect("current code must reject a future metadata schema");
    assert!(matches!(err, Error::Invalid(_)), "unexpected error: {err}");
}

#[test]
fn reopening_current_schema_is_idempotent() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    drop(Meta::open(&path).unwrap());
    drop(Meta::open(&path).unwrap());

    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, CURRENT_SCHEMA_VERSION);
}

// Compatibility rejection must be side-effect free: opening a future schema
// must not switch journal modes or create WAL state before returning the error.
// Couple the fixture to CURRENT_SCHEMA_VERSION so migrations cannot stale it.
#[test]
fn newer_schema_rejection_does_not_mutate_journal_mode() {
    let d = tempdir().unwrap();
    let path = d.path().join("future.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, 0)",
        [CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();
    let before: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(before.to_ascii_lowercase(), "delete");
    drop(conn);

    let err = Meta::open(&path)
        .err()
        .expect("future schema must fail before durability mutation");
    assert!(matches!(err, Error::Invalid(_)), "unexpected error: {err}");

    let conn = Connection::open(&path).unwrap();
    let after: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(after.to_ascii_lowercase(), "delete");
    assert!(!d.path().join("future.sqlite-wal").exists());
}
