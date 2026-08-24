//! I10: Contribution reachability is a monotonic set-union join.

mod support;

use forge_store::Store;
use forge_types::{CasResult, ObjectId, ObjectType};
use std::collections::BTreeSet;
use support::Fixture;

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
    let fixture = Fixture::new();
    let alice = fixture.agent("alice");
    let bob = fixture.agent("bob");

    let alice_ns = fixture.session(&alice, "main");
    fixture
        .forge
        .write(&alice, &alice_ns, "/alice.txt", b"alice", false)
        .unwrap();
    let (alice_ref, alice_commit) = updated(
        fixture
            .forge
            .checkin(&alice, &alice_ns, "/", "alice")
            .unwrap(),
    );

    let bob_ns = fixture.session(&bob, "main");
    fixture
        .forge
        .write(&bob, &bob_ns, "/bob.txt", b"bob", false)
        .unwrap();
    let (bob_ref, bob_commit) = updated(fixture.forge.checkin(&bob, &bob_ns, "/", "bob").unwrap());

    let store = Store::open(&fixture.path().join(".forge")).unwrap();
    let alice_contribution = store.get_commit(alice_commit).unwrap().contrib.unwrap();
    let bob_contribution = store.get_commit(bob_commit).unwrap().contrib.unwrap();
    let expected = BTreeSet::from([alice_contribution, bob_contribution]);

    fixture
        .forge
        .branch(&fixture.root, "main", "joins/alice-bob")
        .unwrap();
    fixture
        .forge
        .branch(&fixture.root, "main", "joins/bob-alice")
        .unwrap();

    let (_, ab_first) = updated(
        fixture
            .forge
            .merge(&fixture.root, "joins/alice-bob", &alice_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, ab_first),
        BTreeSet::from([alice_contribution])
    );
    let (_, ab) = updated(
        fixture
            .forge
            .merge(&fixture.root, "joins/alice-bob", &bob_ref, None)
            .unwrap(),
    );

    let (_, ba_first) = updated(
        fixture
            .forge
            .merge(&fixture.root, "joins/bob-alice", &bob_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, ba_first),
        BTreeSet::from([bob_contribution])
    );
    let (_, ba) = updated(
        fixture
            .forge
            .merge(&fixture.root, "joins/bob-alice", &alice_ref, None)
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
        fixture
            .forge
            .merge(&fixture.root, "joins/alice-bob", &alice_ref, None)
            .unwrap(),
    );
    assert_eq!(
        contribution_set(&store, repeated),
        expected,
        "joining an already reachable receipt must be idempotent"
    );
}
