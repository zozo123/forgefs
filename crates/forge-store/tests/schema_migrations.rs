use forge_store::Meta;
use forge_types::Error;
use rusqlite::Connection;
use tempfile::tempdir;

// Metadata schema versioning is deliberately separate from immutable object format versioning.
#[test]
fn fresh_metadata_records_schema_v1() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let meta = Meta::open(&path).unwrap();
    drop(meta);

    let conn = Connection::open(&path).unwrap();
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, vec![1]);
}

#[test]
fn newer_metadata_schema_fails_closed() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let meta = Meta::open(&path).unwrap();
    drop(meta);

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (2, 0)",
        [],
    )
    .unwrap();
    drop(conn);

    let err = Meta::open(&path)
        .err()
        .expect("v1 code must reject v2 metadata");
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
    assert_eq!(n, 1);
}

#[test]
fn newer_schema_rejection_does_not_mutate_journal_mode() {
    let d = tempdir().unwrap();
    let path = d.path().join("future.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);\
         INSERT INTO schema_migrations (version, applied_ms) VALUES (2, 0);",
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
