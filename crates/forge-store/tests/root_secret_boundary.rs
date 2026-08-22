use forge_store::Meta;
use rusqlite::Connection;
use tempfile::tempdir;

// Mutable metadata may carry only public trust material; minting authority stays
// in the protected key directory and must never be recoverable from SQLite.
#[test]
fn mutable_sqlite_never_retains_capability_minting_secret() {
    let d = tempdir().unwrap();
    let db = d.path().join("meta.sqlite");
    let meta = Meta::open(&db).unwrap();
    let pk = [9u8; 32];
    meta.set_cap_root(&pk).unwrap();
    drop(meta);

    let conn = Connection::open(&db).unwrap();
    let (hmac, stored_pk): (Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT hmac_key, seal_pub FROM cap_root WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(hmac.is_empty(), "root minting secret leaked into SQLite");
    assert_eq!(stored_pk, pk);

    // Opening a legacy DB scrubs an old secret copy in place.
    conn.execute(
        "UPDATE cap_root SET hmac_key=?1 WHERE id=1",
        [vec![7u8; 32]],
    )
    .unwrap();
    drop(conn);
    drop(Meta::open(&db).unwrap());
    let conn = Connection::open(&db).unwrap();
    let hmac: Vec<u8> = conn
        .query_row("SELECT hmac_key FROM cap_root WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert!(hmac.is_empty());
}
