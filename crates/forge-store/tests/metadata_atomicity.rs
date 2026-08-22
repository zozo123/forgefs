use forge_store::Meta;
use forge_types::ObjectId;
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn insert_ref_rolls_back_if_reflog_write_fails() {
    let d = tempdir().unwrap();
    let db = d.path().join("meta.sqlite");
    let meta = Meta::open(&db).unwrap();

    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_reflog BEFORE INSERT ON reflog BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();

    let err = meta
        .insert_ref(
            "atomic-test",
            ObjectId([7; 32]),
            "commit",
            false,
            false,
            "test",
            "test",
        )
        .unwrap_err();
    assert!(format!("{err:?}").contains("boom"));
    assert!(
        meta.get_ref("atomic-test").unwrap().is_none(),
        "ref row must roll back with its reflog"
    );
}
