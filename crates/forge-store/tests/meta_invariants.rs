use forge_store::Meta;
use forge_types::{Error, ObjectId};
use rusqlite::Connection;
use tempfile::tempdir;

fn oid(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

#[test]
fn ref_and_reflog_publish_atomically() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_reflog BEFORE INSERT ON reflog BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta
        .insert_ref("heads/a", oid(1), "commit", false, false, "a", "test")
        .is_err());
    assert!(meta.get_ref("heads/a").unwrap().is_none());
}

#[test]
fn reserved_names_are_typed_and_tags_are_seal_only() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    assert!(matches!(
        meta.insert_ref("heads/a", oid(1), "tree", false, false, "a", "x"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        meta.insert_ref("conflicts/x", oid(2), "commit", false, false, "a", "x"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        meta.insert_ref("tags/fake", oid(3), "snapshot", true, true, "a", "x"),
        Err(Error::Denied(_))
    ));
}

#[test]
fn malformed_names_and_kind_changes_fail() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    for bad in ["", "/abs", "a/", "a//b", "a/./b", "a/../b", "a:b", "a\\b"] {
        assert!(
            matches!(
                meta.insert_ref(bad, oid(1), "commit", false, false, "a", "x"),
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    meta.insert_ref("heads/a", oid(1), "commit", false, false, "a", "x")
        .unwrap();
    assert!(matches!(
        meta.cas_ref("heads/a", oid(1), oid(2), "tree", "a", "a", false),
        Err(Error::Invalid(_))
    ));
}
