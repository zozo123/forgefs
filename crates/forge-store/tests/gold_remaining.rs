use forge_core::{Commit, Tree};
use forge_store::{Meta, Store};
use forge_types::{CasResult, Error, ObjectId};
use rusqlite::Connection;
use tempfile::tempdir;

fn oid(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

#[test]
fn root_minting_secret_never_lives_in_sqlite() {
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
    assert!(hmac.is_empty());
    assert_eq!(stored_pk, pk);

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

#[test]
fn session_creation_is_one_transaction() {
    let d = tempdir().unwrap();
    let db = d.path().join("meta.sqlite");
    let meta = Meta::open(&db).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_mount BEFORE INSERT ON mounts BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    drop(conn);

    let live = "heads/agents/a/ns1";
    assert!(meta.create_session("ns1", "a", oid(1), live, true).is_err());
    assert!(matches!(meta.get_namespace("ns1"), Err(Error::NotFound(_))));
    assert!(meta.get_ref(live).unwrap().is_none());
    assert!(meta.list_mounts("ns1").unwrap().is_empty());
}

#[test]
fn checkin_publication_and_session_state_roll_back_together() {
    let d = tempdir().unwrap();
    let db = d.path().join("meta.sqlite");
    let meta = Meta::open(&db).unwrap();
    let live = "heads/agents/a/ns1";
    meta.create_session("ns1", "a", oid(1), live, false).unwrap();
    meta.overlay_upsert("ns1", "/", "f", Some(oid(9)), false)
        .unwrap();
    meta.observe("ns1", "/", "seen", oid(8)).unwrap();

    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_pin BEFORE UPDATE OF pinned_oid ON namespaces BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    drop(conn);

    assert!(meta
        .cas_ref_session("ns1", "/", live, oid(1), oid(2), "commit", "a", "a")
        .is_err());
    assert_eq!(meta.get_ref(live).unwrap().unwrap().oid, oid(1));
    assert_eq!(meta.get_namespace("ns1").unwrap().pinned_oid, Some(oid(1)));
    assert_eq!(meta.overlay_list("ns1", "/").unwrap().len(), 1);
    assert_eq!(meta.observations("ns1").unwrap().len(), 1);
}

#[test]
fn lost_cas_fork_retargets_the_session_atomically() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("meta.sqlite")).unwrap();
    let live = "heads/agents/a/ns1";
    meta.create_session("ns1", "a", oid(1), live, false).unwrap();
    meta.cas_ref(live, oid(1), oid(3), "commit", "other", "other", false)
        .unwrap();
    meta.overlay_upsert("ns1", "/", "f", Some(oid(9)), false)
        .unwrap();

    let result = meta
        .cas_ref_session("ns1", "/", live, oid(1), oid(2), "commit", "a", "a")
        .unwrap();
    let CasResult::Forked { fork, ours, theirs, .. } = result else {
        panic!("expected fork");
    };
    assert_eq!(ours, oid(2));
    assert_eq!(theirs, oid(3));
    assert_eq!(meta.get_ref(&fork).unwrap().unwrap().oid, oid(2));
    let ns = meta.get_namespace("ns1").unwrap();
    assert_eq!(ns.pinned_oid, Some(oid(2)));
    assert_eq!(ns.live_ref.as_deref(), Some(fork.as_str()));
    assert_eq!(
        meta.list_mounts("ns1")
            .unwrap()
            .iter()
            .find(|m| m.path == "/")
            .unwrap()
            .spec,
        format!("ref:{fork}")
    );
    assert!(meta.overlay_list("ns1", "/").unwrap().is_empty());
}

#[test]
fn provenance_batch_is_atomic_and_old_tree_type_is_strict() {
    let d = tempdir().unwrap();
    let db = d.path().join("meta.sqlite");
    let meta = Meta::open(&db).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_second_intro BEFORE INSERT ON object_intro \
         WHEN (SELECT COUNT(*) FROM object_intro) >= 1 \
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    drop(conn);
    assert!(meta
        .intro_insert_many(&[oid(1), oid(2)], oid(9), "agent")
        .is_err());
    assert!(meta.intro_get(oid(1)).unwrap().is_none());
    assert!(meta.intro_get(oid(2)).unwrap().is_none());

    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let new_tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let old_blob = store.put_blob_data(b"old-is-not-a-tree").unwrap();
    let commit = store
        .put_commit(&Commit {
            tree: new_tree,
            parents: vec![],
            agent: "a".into(),
            msg: "m".into(),
            ts: 1,
            landmark: false,
        })
        .unwrap();
    assert!(matches!(
        store.record_intros(Some(old_blob), new_tree, commit, "a"),
        Err(Error::Corrupt(_))
    ));
}
