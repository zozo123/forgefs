use forge_api::Forge;
use forge_core::{Commit, Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId};
use tempfile::tempdir;

fn store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join(".forge")).unwrap()
}

fn tree(store: &Store, data: &[u8]) -> ObjectId {
    let blob = store.put_blob_data(data).unwrap();
    store
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "same.txt".into(),
                kind: EntryKind::Blob,
                id: blob,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap()
}

fn commit(store: &Store, tree: ObjectId, parent: ObjectId, n: u64) -> ObjectId {
    store
        .put_commit(&Commit {
            tree,
            parents: vec![parent],
            agent: "show-test".into(),
            msg: format!("c{n}"),
            ts: n,
            landmark: false,
        })
        .unwrap()
}

#[test]
fn show_conflict_exposes_both_sides_and_typed_path_oids() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    let main = s.meta.get_ref("main").unwrap().unwrap();
    let ours = commit(&s, tree(&s, b"ours"), main.oid, 1);
    let theirs = commit(&s, tree(&s, b"theirs"), main.oid, 2);
    s.meta
        .insert_ref("heads/ours", ours, "commit", false, false, "test", "test")
        .unwrap();
    s.meta
        .insert_ref(
            "heads/theirs",
            theirs,
            "commit",
            false,
            false,
            "test",
            "test",
        )
        .unwrap();

    let err = forge
        .merge(&root, "heads/ours", "heads/theirs", None)
        .unwrap_err();
    let Error::MergeConflict(oid) = err else {
        panic!("expected conflict: {err:?}");
    };
    let conflict = s.get_conflict(oid).unwrap();

    // Raw object inspection is explicit (`oid:`) and remains root/full-read only.
    let rendered = forge.show(&root, &format!("oid:{}", oid.hex())).unwrap();

    assert!(rendered.contains(&format!("conflict {oid}")));
    assert!(rendered.contains(&format!("ours {}", conflict.ours)));
    assert!(rendered.contains(&format!("theirs {}", conflict.theirs)));
    assert!(rendered.contains("path same.txt a="));
    assert!(rendered.contains(" b="));
    assert!(rendered.contains(" base="));
    assert!(rendered.contains("causal "));
}
