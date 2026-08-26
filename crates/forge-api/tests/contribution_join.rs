//! I10: Contribution reachability is a monotonic set-union join.

use forge_api::Forge;
use forge_store::Store;
use forge_types::{CasResult, ObjectId, ObjectType};
use std::collections::BTreeSet;
use tempfile::tempdir;

fn updated(result: CasResult) -> (String, ObjectId) {
    match result {
        CasResult::Updated { name, oid } => (name, oid),
        other => panic!("expected an uncontended update, got {other:?}"),
    }
}

fn contribution_set(store: &Store, commit: ObjectId) -> BTreeSet<ObjectId> {
    store
        .reachable_graph_verified(commit, ObjectType::Commit)
        .unwrap()
        .into_iter()
        .filter_map(|object| (object.object_type == ObjectType::Contribution).then_some(object.id))
        .collect()
}

#[test]
fn opposite_merge_orders_have_the_same_monotonic_contribution_join() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let alice = forge
        .grant(
            &root,
            vec![
                "ops=read,write,branch".into(),
                "agent=alice".into(),
                "ref=main,heads/agents/alice/*".into(),
            ],
        )
        .unwrap();
    let bob = forge
        .grant(
            &root,
            vec![
                "ops=read,write,branch".into(),
                "agent=bob".into(),
                "ref=main,heads/agents/bob/*".into(),
            ],
        )
        .unwrap();
    let store = Store::open(&dir.path().join(".forge")).unwrap();
    let shared_base = store.meta.get_ref("main").unwrap().unwrap().oid;

    let alice_ns = forge.session_open(&alice, "main").unwrap();
    forge
        .write(&alice, &alice_ns, "/alice.txt", b"alice", false)
        .unwrap();
    let (alice_ref, alice_commit) =
        updated(forge.checkin(&alice, &alice_ns, "/", "alice").unwrap());

    let bob_ns = forge.session_open(&bob, "main").unwrap();
    forge
        .write(&bob, &bob_ns, "/bob.txt", b"bob", false)
        .unwrap();
    let (bob_ref, bob_commit) = updated(forge.checkin(&bob, &bob_ns, "/", "bob").unwrap());

    let alice_contribution = store.get_commit(alice_commit).unwrap().contrib.unwrap();
    let bob_contribution = store.get_commit(bob_commit).unwrap().contrib.unwrap();
    assert_eq!(
        store.get_contribution(alice_contribution).unwrap().base,
        shared_base
    );
    assert_eq!(
        store.get_contribution(bob_contribution).unwrap().base,
        shared_base
    );
    assert_eq!(
        store.meta.get_ref("main").unwrap().unwrap().oid,
        shared_base,
        "private checkins must not advance their shared starting ref"
    );
    assert_ne!(
        alice_contribution, bob_contribution,
        "two independent checkins must publish distinct Contribution facts; \
         equal receipts would collapse every set below to one element and let \
         the join assertions pass without witnessing a join"
    );
    let expected = BTreeSet::from([alice_contribution, bob_contribution]);

    forge.branch(&root, "main", "joins/alice-bob").unwrap();
    forge.branch(&root, "main", "joins/bob-alice").unwrap();

    let (_, ab_first) = updated(
        forge
            .merge(&root, "joins/alice-bob", &alice_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, ab_first),
        BTreeSet::from([alice_contribution])
    );
    let (_, ab) = updated(
        forge
            .merge(&root, "joins/alice-bob", &bob_ref, None)
            .unwrap(),
    );

    let (_, ba_first) = updated(
        forge
            .merge(&root, "joins/bob-alice", &bob_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, ba_first),
        BTreeSet::from([bob_contribution])
    );
    let (_, ba) = updated(
        forge
            .merge(&root, "joins/bob-alice", &alice_ref, None)
            .unwrap(),
    );

    assert_eq!(contribution_set(&store, ab), expected);
    assert_eq!(contribution_set(&store, ba), expected);
    assert_eq!(
        store.get_commit(ab).unwrap().tree,
        store.get_commit(ba).unwrap().tree,
        "disjoint content and receipt joins must converge"
    );

    let (_, repeated) = updated(
        forge
            .merge(&root, "joins/alice-bob", &alice_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, repeated),
        expected,
        "joining an already reachable receipt must be idempotent"
    );
}
