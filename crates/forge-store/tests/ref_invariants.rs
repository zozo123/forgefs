use forge_store::Meta;
use forge_types::{Error, ObjectId};
use tempfile::tempdir;

fn oid(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

#[test]
fn generic_ref_api_cannot_forge_reserved_tags() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    let err = meta
        .insert_ref("tags/fake", oid(1), "commit", false, false, "x", "branch")
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_) | Error::Denied(_)), "{err:?}");
}

#[test]
fn reserved_namespaces_enforce_object_kind() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();

    assert!(meta
        .insert_ref(
            "conflicts/main/c1",
            oid(1),
            "commit",
            false,
            false,
            "x",
            "test",
        )
        .is_err());
    assert!(meta
        .insert_ref("heads/a", oid(2), "tree", false, false, "x", "test")
        .is_err());
    assert!(meta
        .insert_ref("forks/a/x/1", oid(3), "snapshot", false, false, "x", "test")
        .is_err());
}

#[test]
fn ref_names_reject_path_tricks() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    for name in ["", "/abs", "tail/", "a//b", "a/../b", "a/./b"] {
        let err = meta
            .insert_ref(name, oid(1), "commit", false, false, "x", "test")
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_) | Error::Denied(_)), "{name}: {err:?}");
    }
}

#[test]
fn cas_cannot_change_head_kind() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    meta.insert_ref("heads/a", oid(1), "commit", false, false, "x", "test")
        .unwrap();
    let err = meta
        .cas_ref("heads/a", oid(1), oid(2), "tree", "x", "x", false)
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_) | Error::Denied(_)), "{err:?}");
}
