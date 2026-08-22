use forge_core::{Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error};
use tempfile::tempdir;

#[test]
fn verified_walk_rejects_tree_edge_to_blob() {
    let d = tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let blob = s.put_blob_data(b"not a tree").unwrap();
    let root = s
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "child".into(),
                kind: EntryKind::Tree,
                id: blob,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap();

    let err = s.reachable_oids_verified(root).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}

#[test]
fn verified_walk_rejects_blob_edge_to_tree() {
    let d = tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let child_tree = s.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let root = s
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "child".into(),
                kind: EntryKind::Blob,
                id: child_tree,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap();

    let err = s.reachable_oids_verified(root).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}
