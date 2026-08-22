use forge_core::{Commit, Tree};
use forge_merge::lca;
use forge_store::Store;
use forge_types::Error;
use tempfile::tempdir;

fn commit(s: &Store, parents: Vec<forge_types::ObjectId>, n: u64) -> forge_types::ObjectId {
    let tree = s.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    s.put_commit(&Commit {
        tree,
        parents,
        agent: "test".into(),
        msg: format!("c{n}"),
        ts: n,
        landmark: false,
        contrib: None,
    })
    .unwrap()
}

#[test]
fn single_best_common_ancestor_is_selected() {
    let d = tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let root = commit(&s, vec![], 0);
    let base = commit(&s, vec![root], 1);
    let a = commit(&s, vec![base], 2);
    let b = commit(&s, vec![base], 3);
    assert_eq!(lca(&s, a, b).unwrap(), Some(base));
}

#[test]
fn criss_cross_does_not_silently_choose_one_of_multiple_bases() {
    let d = tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let root = commit(&s, vec![], 0);
    let a1 = commit(&s, vec![root], 1);
    let b1 = commit(&s, vec![root], 2);
    let a2 = commit(&s, vec![a1, b1], 3);
    let b2 = commit(&s, vec![b1, a1], 4);

    let err = lca(&s, a2, b2).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
}

#[test]
fn malformed_parent_edge_fails_closed() {
    let d = tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let not_a_commit = s.put_blob_data(b"blob parent").unwrap();
    let a = commit(&s, vec![not_a_commit], 1);
    let b = commit(&s, vec![not_a_commit], 2);

    assert!(lca(&s, a, b).is_err());
}
