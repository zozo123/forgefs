//! I11 conflict resolution without a FORMAT VERSION bump (#15 PR1).
//!
//! Detection already publishes a Conflict and a conflicts/<into>/<ulid> ref.
//! These tests pin the resolve verb: choice of ours/theirs/scoped tree,
//! foreign-conflict refusal, head-moved exit 4, and conflict-ref retirement.

use forge_api::{Forge, ResolveChoice};
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
            agent: "resolve-test".into(),
            msg: format!("c{n}"),
            ts: n,
            landmark: false,
            contrib: None,
        })
        .unwrap()
}

fn conflict_pair(dir: &std::path::Path) -> (Forge, ObjectId, ObjectId, ObjectId) {
    let forge = Forge::init(dir).unwrap();
    let root = forge.root_cap().unwrap();
    let s = store(dir);
    let main = s.meta.get_ref("main").unwrap().unwrap();
    let ours_tree = tree(&s, b"ours");
    let theirs_tree = tree(&s, b"theirs");
    let ours = commit(&s, ours_tree, main.oid, 1);
    let theirs = commit(&s, theirs_tree, main.oid, 2);
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
    (forge, oid, ours_tree, theirs_tree)
}

#[test]
fn resolve_ours_advances_into_and_retires_conflict_ref() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, ours_tree, _) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    let before_refs = s.meta.list_refs().unwrap();
    let conflict_ref = before_refs
        .iter()
        .find(|r| r.oid == conflict_oid && r.kind == "conflict")
        .expect("conflict ref published")
        .name
        .clone();

    let published = forge
        .resolve(&root, conflict_oid, "heads/ours", ResolveChoice::Ours)
        .unwrap();

    assert_eq!(published.into, "heads/ours");
    assert_eq!(published.retired_conflict, conflict_ref);
    assert!(published.resolution_ref.starts_with("resolutions/"));
    let tip = s.meta.get_ref("heads/ours").unwrap().unwrap();
    assert_eq!(tip.oid, published.commit);
    let commit = s.get_commit(published.commit).unwrap();
    assert_eq!(commit.tree, ours_tree);
    let conflict = s.get_conflict(conflict_oid).unwrap();
    assert_eq!(commit.parents, conflict.causal);
    assert!(s.meta.get_ref(&conflict_ref).unwrap().is_none());
    let binding = s.meta.get_ref(&published.resolution_ref).unwrap().unwrap();
    assert_eq!(binding.oid, published.commit);
    assert_eq!(binding.kind, "commit");
}

#[test]
fn resolve_theirs_takes_the_from_side_tree() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, theirs_tree) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());

    let published = forge
        .resolve(&root, conflict_oid, "heads/ours", ResolveChoice::Theirs)
        .unwrap();

    let commit = s.get_commit(published.commit).unwrap();
    assert_eq!(commit.tree, theirs_tree);
}

#[test]
fn resolve_tree_must_stay_inside_conflict_paths() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, _) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    let before = s.meta.get_ref("heads/ours").unwrap().unwrap().oid;

    let foreign = s.put_blob_data(b"foreign").unwrap();
    let escaped = s
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "other.txt".into(),
                kind: EntryKind::Blob,
                id: foreign,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap();

    let err = forge
        .resolve(
            &root,
            conflict_oid,
            "heads/ours",
            ResolveChoice::Tree(escaped),
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert!(
        matches!(&err, Error::Invalid(msg) if msg.contains("escapes conflict scope")),
        "{err}"
    );
    assert_eq!(
        s.meta.get_ref("heads/ours").unwrap().unwrap().oid,
        before,
        "into must stay put on a refused resolve"
    );
    assert!(s
        .meta
        .list_refs()
        .unwrap()
        .iter()
        .any(|r| r.oid == conflict_oid && r.kind == "conflict"));
}

#[test]
fn resolve_tree_may_pick_theirs_blob_on_the_conflict_path() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, theirs_tree) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    // Choosing the theirs tree wholesale is in scope: it only differs from the
    // auto-merge (which kept ours on the conflict path) at same.txt.
    let published = forge
        .resolve(
            &root,
            conflict_oid,
            "heads/ours",
            ResolveChoice::Tree(theirs_tree),
        )
        .unwrap();
    assert_eq!(s.get_commit(published.commit).unwrap().tree, theirs_tree);
}

#[test]
fn resolve_foreign_into_is_refused() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, _) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    // main is not a causal tip of this conflict.
    let err = forge
        .resolve(&root, conflict_oid, "main", ResolveChoice::Ours)
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert!(s
        .meta
        .list_refs()
        .unwrap()
        .iter()
        .any(|r| r.oid == conflict_oid && r.kind == "conflict"));
}

#[test]
fn resolve_moved_head_is_stale_not_a_fork() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, _) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let s = store(d.path());
    let moved = commit(
        &s,
        tree(&s, b"moved"),
        s.meta.get_ref("heads/ours").unwrap().unwrap().oid,
        9,
    );
    s.meta
        .cas_ref(
            "heads/ours",
            s.meta.get_ref("heads/ours").unwrap().unwrap().oid,
            moved,
            "commit",
            "mover",
            "mover",
            false,
        )
        .unwrap();
    let forks_before = s
        .meta
        .list_refs()
        .unwrap()
        .iter()
        .filter(|r| r.name.starts_with("forks/"))
        .count();

    let err = forge
        .resolve(&root, conflict_oid, "heads/ours", ResolveChoice::Ours)
        .unwrap_err();
    assert!(
        matches!(err, Error::StaleObservation { .. }),
        "moved head must be exit-4 stale, got {err:?}"
    );
    assert_eq!(err.code(), "stale_observation");
    assert_eq!(err.exit_code(), 4);
    let forks_after = s
        .meta
        .list_refs()
        .unwrap()
        .iter()
        .filter(|r| r.name.starts_with("forks/"))
        .count();
    assert_eq!(forks_before, forks_after, "resolve must not mint a fork");
}

#[test]
fn read_only_cap_cannot_resolve() {
    let d = tempdir().unwrap();
    let (forge, conflict_oid, _, _) = conflict_pair(d.path());
    let root = forge.root_cap().unwrap();
    let read_only = forge
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "allow=read:heads/ours".into(),
                "allow=read:conflicts/*".into(),
            ],
        )
        .unwrap();
    let err = forge
        .resolve(&read_only, conflict_oid, "heads/ours", ResolveChoice::Ours)
        .unwrap_err();
    assert_eq!(err.code(), "denied");
}
