use forge_core::{Tree, TreeEntry};
use forge_merge::{three_way, MergeOutcome};
use forge_store::Store;
use forge_types::{EntryKind, ObjectId};
use tempfile::tempdir;

fn blob(name: &str, id: ObjectId, exec: bool) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        kind: EntryKind::Blob,
        id,
        exec,
    }
}

fn put_tree(store: &Store, entries: Vec<TreeEntry>) -> ObjectId {
    store.put_tree(&Tree::new(entries).unwrap()).unwrap()
}

fn assert_conflict(outcome: MergeOutcome) {
    match outcome {
        MergeOutcome::Conflict(conflict) => {
            assert_eq!(conflict.paths.len(), 1);
            assert_eq!(conflict.paths[0].path, "tool.sh");
        }
        MergeOutcome::Tree(tree) => panic!("delete-vs-metadata must conflict, got {tree}"),
    }
}

#[test]
fn delete_vs_chmod_conflicts_in_both_directions() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let tool = store.put_blob_data(b"#!/bin/sh\n").unwrap();
    let base = put_tree(&store, vec![blob("tool.sh", tool, false)]);
    let chmod = put_tree(&store, vec![blob("tool.sh", tool, true)]);
    let deleted = put_tree(&store, vec![]);

    assert_conflict(three_way(&store, Some(base), chmod, deleted).unwrap());
    assert_conflict(three_way(&store, Some(base), deleted, chmod).unwrap());
}

#[test]
fn unchanged_vs_delete_still_resolves_to_the_same_deletion() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let tool = store.put_blob_data(b"#!/bin/sh\n").unwrap();
    let base = put_tree(&store, vec![blob("tool.sh", tool, false)]);
    let unchanged = put_tree(&store, vec![blob("tool.sh", tool, false)]);
    let deleted = put_tree(&store, vec![]);

    for outcome in [
        three_way(&store, Some(base), unchanged, deleted).unwrap(),
        three_way(&store, Some(base), deleted, unchanged).unwrap(),
    ] {
        match outcome {
            MergeOutcome::Tree(tree) => assert_eq!(tree, deleted),
            MergeOutcome::Conflict(conflict) => {
                panic!("unchanged-vs-delete must be clean: {conflict:?}")
            }
        }
    }
}
