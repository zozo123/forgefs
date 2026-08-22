use forge_store::{meta::SCHEMA, Meta};
use rusqlite::{params, Connection};
use tempfile::tempdir;

fn version(path: &std::path::Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn new_database_is_atomically_initialized_at_schema_v1() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    Meta::open(&path).unwrap();
    assert_eq!(version(&path), 1);
}

#[test]
fn legacy_unversioned_database_migrates_without_losing_rows() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES ('legacy', ?1, 'commit', 0, 0, 7)",
        params![vec![9u8; 32]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO cap_root (id, hmac_key, seal_pub) VALUES (1, ?1, ?2)",
        params![vec![7u8; 32], vec![8u8; 32]],
    )
    .unwrap();
    drop(conn);

    let meta = Meta::open(&path).unwrap();
    let row = meta.get_ref("legacy").unwrap().unwrap();
    assert_eq!(row.oid.as_bytes(), &[9u8; 32]);
    assert_eq!(version(&path), 1);

    let conn = Connection::open(&path).unwrap();
    let hmac_len: i64 = conn
        .query_row("SELECT length(hmac_key) FROM cap_root WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hmac_len, 0, "legacy root HMAC material must be scrubbed during migration");
}

#[test]
fn future_metadata_version_is_refused_without_mutation() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.pragma_update(None, "user_version", 999i64).unwrap();
    drop(conn);

    assert!(Meta::open(&path).is_err());
    assert_eq!(version(&path), 999);
}

#[test]
fn incompatible_legacy_schema_fails_and_does_not_claim_upgrade() {
    let d = tempdir().unwrap();
    let path = d.path().join("meta.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE refs(name TEXT PRIMARY KEY);")
        .unwrap();
    drop(conn);

    assert!(Meta::open(&path).is_err());
    assert_eq!(version(&path), 0);
}
