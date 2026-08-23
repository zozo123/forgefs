use forge_api::{Forge, RAW_MERGE_RESOLUTION_DISABLED};
use forge_core::{Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId};
use tempfile::tempdir;

fn store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join(".forge")).unwrap()
}

fn unrelated_tree(store: &Store) -> ObjectId {
    let blob = store.put_blob_data(b"unrelated").unwrap();
    store
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: "unrelated.txt".into(),
                kind: EntryKind::Blob,
                id: blob,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap()
}

fn assert_resolution_disabled(error: Error) {
    assert_eq!(error.code(), "invalid");
    match error {
        Error::Invalid(message) => assert_eq!(message, RAW_MERGE_RESOLUTION_DISABLED),
        other => panic!("expected fail-closed merge resolution error, got {other:?}"),
    }
}

#[test]
fn raw_resolved_tree_is_rejected_without_advancing_destination() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge
        .branch(&root, "main", "heads/agents/alice/source")
        .unwrap();

    let injected = unrelated_tree(&store(d.path()));
    let (before, before_commit) = forge.peel_commit("main").unwrap();
    assert_ne!(before_commit.tree, injected);

    let error = forge
        .merge(&root, "main", "heads/agents/alice/source", Some(injected))
        .unwrap_err();
    assert_resolution_disabled(error);

    let (after, after_commit) = forge.peel_commit("main").unwrap();
    assert_eq!(after, before);
    assert_eq!(after_commit.tree, before_commit.tree);
}

#[test]
fn scoped_capability_cannot_smuggle_an_unreadable_tree() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge
        .branch(&root, "main", "heads/agents/alice/source")
        .unwrap();

    let injected = unrelated_tree(&store(d.path()));
    let before = forge.peel_commit("main").unwrap().0;

    // Authorization remains the first gate: a read-only capability does not
    // gain information or merge authority by supplying the legacy argument.
    let read_only = forge
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "allow=read:heads/agents/alice/source".into(),
            ],
        )
        .unwrap();
    let error = forge
        .merge(
            &read_only,
            "main",
            "heads/agents/alice/source",
            Some(injected),
        )
        .unwrap_err();
    assert!(matches!(&error, Error::Denied(_)), "{error:?}");

    // The integrator may read the source and merge into main, but its scoped
    // authority must not make an otherwise unreachable Tree OID injectable.
    let integrator = forge.integrator_cap().unwrap();
    let error = forge
        .merge(
            &integrator,
            "main",
            "heads/agents/alice/source",
            Some(injected),
        )
        .unwrap_err();
    assert_resolution_disabled(error);
    assert_eq!(forge.peel_commit("main").unwrap().0, before);
}
