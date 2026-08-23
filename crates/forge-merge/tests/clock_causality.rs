use forge_core::{Commit, Contribution, Tree};
use forge_merge::{lca, merge_bases};
use forge_store::Store;
use forge_types::ObjectId;
use tempfile::tempdir;

fn commit(store: &Store, tree: ObjectId, parents: Vec<ObjectId>, ts: u64, msg: &str) -> ObjectId {
    store
        .put_commit(&Commit {
            tree,
            parents,
            agent: "clock-skew-test".into(),
            msg: msg.into(),
            ts,
            landmark: false,
            contrib: None,
        })
        .unwrap()
}

fn commit_with_contribution(
    store: &Store,
    tree: ObjectId,
    parent: ObjectId,
    contribution_ts: u64,
    msg: &str,
) -> ObjectId {
    let contrib = store
        .put_contribution(&Contribution {
            base: parent,
            tree,
            parents: vec![parent],
            reads: vec![],
            writes: vec![],
            agent: "clock-skew-test".into(),
            ts: contribution_ts,
        })
        .unwrap();
    store
        .put_commit(&Commit {
            tree,
            parents: vec![parent],
            agent: "clock-skew-test".into(),
            msg: msg.into(),
            ts: 123,
            landmark: false,
            contrib: Some(contrib),
        })
        .unwrap()
}

#[test]
fn graph_causality_beats_inverted_wall_clocks() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();

    // Deliberately make every causal generation move backward in wall-clock time.
    let root = commit(&store, tree, vec![], 4_000_000_000, "root");
    let base = commit(&store, tree, vec![root], 3_000_000_000, "base");
    let ours = commit(&store, tree, vec![base], 2, "ours");
    let theirs = commit(&store, tree, vec![base], 1, "theirs");

    assert_eq!(merge_bases(&store, ours, theirs).unwrap(), vec![base]);
    assert_eq!(lca(&store, ours, theirs).unwrap(), Some(base));
}

#[test]
fn graph_causality_beats_extreme_future_clock_skew() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();

    // The root looks newest by timestamp; the actual best base is its descendant.
    let root = commit(&store, tree, vec![], u64::MAX, "future-root");
    let base = commit(&store, tree, vec![root], 0, "real-base");
    let ours = commit(&store, tree, vec![base], u64::MAX - 1, "ours");
    let theirs = commit(&store, tree, vec![base], 7, "theirs");

    assert_eq!(merge_bases(&store, ours, theirs).unwrap(), vec![base]);
}

#[test]
fn contribution_timestamps_do_not_order_merge_bases() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();

    let root = commit(&store, tree, vec![], 123, "root");
    // Make the causal base look newest and both children look arbitrarily older/newer
    // through their Contribution clocks. Merge-base selection must still follow only
    // Commit.parents.
    let base = commit_with_contribution(&store, tree, root, u64::MAX, "base");
    let ours = commit_with_contribution(&store, tree, base, 0, "ours");
    let theirs = commit_with_contribution(&store, tree, base, 1, "theirs");

    assert_eq!(merge_bases(&store, ours, theirs).unwrap(), vec![base]);
    assert_eq!(lca(&store, ours, theirs).unwrap(), Some(base));
}

#[test]
fn identical_commit_validation_is_not_timestamp_ordering() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let commit = commit(&store, tree, vec![], u64::MAX, "replayed-looking");

    // Equal heads are accepted because the object is a valid commit, not because
    // its timestamp is fresh or plausible.
    assert_eq!(merge_bases(&store, commit, commit).unwrap(), vec![commit]);
}
