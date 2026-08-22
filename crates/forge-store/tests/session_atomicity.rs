use forge_store::Meta;
use forge_types::{CasResult, ObjectId};
use rusqlite::Connection;
use tempfile::tempdir;

fn oid(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

fn seed(meta: &Meta, ns: &str, ref_name: &str, pin: ObjectId, current: ObjectId) {
    meta.insert_ref(ref_name, current, "commit", false, false, "a", "seed")
        .unwrap();
    meta.insert_namespace(ns, "a", pin, ref_name).unwrap();
    meta.insert_mount(ns, "/", &format!("ref:{ref_name}"), "rw")
        .unwrap();
    meta.overlay_upsert(ns, "/", "x", Some(oid(9)), false)
        .unwrap();
    meta.observe(ns, "/", "read", oid(8)).unwrap();
}

#[test]
fn session_creation_rolls_back_if_mount_insert_fails() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_mount BEFORE INSERT ON mounts BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    let live = "heads/agents/a/ns1";
    assert!(meta.create_session("ns1", "a", oid(1), live, true).is_err());
    assert!(meta.get_namespace("ns1").is_err());
    assert!(meta.get_ref(live).unwrap().is_none());
    assert!(meta.reflog(live, 10).unwrap().is_empty());
}

#[test]
fn failed_checkin_cleanup_rolls_back_ref_and_session_state() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(1));
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_overlay BEFORE DELETE ON overlay BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta
        .cas_ref_session("shared", oid(1), oid(2), "a", "a", "ns", "/")
        .is_err());
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(1));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(1)));
    assert_eq!(meta.overlay_list("ns", "/").unwrap().len(), 1);
    assert_eq!(meta.observations("ns").unwrap().len(), 1);
}

#[test]
fn successful_checkin_moves_ref_and_clears_session_state_together() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(1));
    let r = meta
        .cas_ref_session("shared", oid(1), oid(2), "a", "a", "ns", "/")
        .unwrap();
    assert!(matches!(r, CasResult::Updated { .. }));
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(2));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(2)));
    assert!(meta.overlay_list("ns", "/").unwrap().is_empty());
    assert!(meta.observations("ns").unwrap().is_empty());
}

#[test]
fn stale_checkin_forks_and_retargets_mount_atomically() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(2));
    let r = meta
        .cas_ref_session("shared", oid(1), oid(3), "a", "a", "ns", "/")
        .unwrap();
    let CasResult::Forked { fork, .. } = r else {
        panic!("expected fork")
    };
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(2));
    assert_eq!(meta.get_ref(&fork).unwrap().unwrap().oid, oid(3));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(3)));
    let mounts = meta.list_mounts("ns").unwrap();
    assert_eq!(
        mounts.iter().find(|m| m.path == "/").unwrap().spec,
        format!("ref:{fork}")
    );
    assert!(meta.overlay_list("ns", "/").unwrap().is_empty());
    assert!(meta.observations("ns").unwrap().is_empty());
}

#[test]
fn provenance_batch_rolls_back_all_rows_on_failure() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_second_intro BEFORE INSERT ON object_intro WHEN (SELECT count(*) FROM object_intro) >= 1 BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta
        .intro_insert_many(&[oid(1), oid(2)], oid(3), "a")
        .is_err());
    assert!(meta.intro_get(oid(1)).unwrap().is_none());
    assert!(meta.intro_get(oid(2)).unwrap().is_none());
}
