use forge_core::{Commit, Contribution, ContributionRead, Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectType};
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

#[test]
fn verified_commit_graph_follows_and_type_checks_contributions() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let base = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            agent: "init".into(),
            msg: "base".into(),
            ts: 1,
            landmark: false,
            contrib: None,
        })
        .unwrap();
    let read = store.put_blob_data(b"observed").unwrap();
    let repeated_reads = (0..128)
        .map(|n| ContributionRead {
            path: format!("/observed/{n:03}"),
            id: read,
        })
        .collect();
    let contribution = store
        .put_contribution(&Contribution {
            base,
            tree,
            parents: vec![base],
            reads: repeated_reads,
            writes: vec!["/result".into()],
            agent: "agent".into(),
            ts: 2,
        })
        .unwrap();
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![base],
            agent: "agent".into(),
            msg: "result".into(),
            ts: 2,
            landmark: false,
            contrib: Some(contribution),
        })
        .unwrap();

    let graph = store
        .reachable_graph_verified(commit, ObjectType::Commit)
        .unwrap();
    assert!(graph.iter().any(|object| {
        object.id == contribution && object.object_type == ObjectType::Contribution
    }));
    assert_eq!(
        graph
            .iter()
            .filter(|object| object.id == read && object.object_type == ObjectType::Blob)
            .count(),
        1
    );

    let malformed = store
        .put_contribution(&Contribution {
            base,
            tree,
            parents: vec![base],
            reads: vec![ContributionRead {
                path: "/not-a-blob".into(),
                id: tree,
            }],
            writes: vec![],
            agent: "agent".into(),
            ts: 3,
        })
        .unwrap();
    let malformed_commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![base],
            agent: "agent".into(),
            msg: "malformed".into(),
            ts: 3,
            landmark: false,
            contrib: Some(malformed),
        })
        .unwrap();
    let error = store
        .reachable_graph_verified(malformed_commit, ObjectType::Commit)
        .unwrap_err();
    assert!(
        matches!(&error, Error::Corrupt(detail) if detail.contains("expected blob") && detail.contains("found tree")),
        "{error:?}"
    );
}
