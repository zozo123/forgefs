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
            contrib: None,
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

/// I12: which side of a conflict is `ours` comes from the ref arguments and the
/// parent DAG, never from `Commit.ts`. The `--into` side here carries the older
/// wall clock, so a merge that ordered its inputs by timestamp would silently
/// relabel both sides and mis-attribute the conflicting blobs.
#[test]
fn i12_conflict_sides_follow_ref_arguments_not_commit_ts() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    let main = s.meta.get_ref("main").unwrap().unwrap();
    let ours_tree = tree(&s, b"ours");
    let theirs_tree = tree(&s, b"theirs");
    // Both sides branch off `main`, so the DAG makes neither older; only the
    // advisory clocks disagree, and they disagree as loudly as possible.
    let ours = commit(&s, ours_tree, main.oid, 1);
    let theirs = commit(&s, theirs_tree, main.oid, u64::MAX);
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

    assert_eq!(
        conflict.ours, ours_tree,
        "`ours` must be the --into side, not the side with the newest Commit.ts"
    );
    assert_eq!(
        conflict.theirs, theirs_tree,
        "`theirs` must be the --from side, not the side with the oldest Commit.ts"
    );
    assert_eq!(
        conflict.causal,
        vec![ours, theirs],
        "causal order is (into, from)"
    );
    assert_eq!(conflict.paths.len(), 1);
    let path = &conflict.paths[0];
    assert_eq!(path.path, "same.txt");
    assert_eq!(
        path.a,
        Some(s.get_tree(ours_tree).unwrap().entries[0].id),
        "side a is the --into side blob"
    );
    assert_eq!(
        path.b,
        Some(s.get_tree(theirs_tree).unwrap().entries[0].id),
        "side b is the --from side blob"
    );
}
