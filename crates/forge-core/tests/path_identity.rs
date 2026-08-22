use forge_core::{Tree, TreeEntry};
use forge_types::{EntryKind, ObjectId};

#[test]
fn i16_unicode_normalization_does_not_change_tree_identity() {
    let composed = "caf\u{00e9}";
    let decomposed = "cafe\u{0301}";
    assert_ne!(composed.as_bytes(), decomposed.as_bytes());

    let tree = Tree::new(vec![
        TreeEntry {
            name: composed.into(),
            kind: EntryKind::Blob,
            id: ObjectId([1; 32]),
            exec: false,
        },
        TreeEntry {
            name: decomposed.into(),
            kind: EntryKind::Blob,
            id: ObjectId([2; 32]),
            exec: false,
        },
    ])
    .unwrap();

    assert_eq!(tree.entries.len(), 2);
    assert_eq!(tree.get(composed).unwrap().id, ObjectId([1; 32]));
    assert_eq!(tree.get(decomposed).unwrap().id, ObjectId([2; 32]));
}

#[test]
fn i16_case_distinct_names_remain_distinct() {
    let tree = Tree::new(vec![
        TreeEntry {
            name: "Foo".into(),
            kind: EntryKind::Blob,
            id: ObjectId([3; 32]),
            exec: false,
        },
        TreeEntry {
            name: "foo".into(),
            kind: EntryKind::Blob,
            id: ObjectId([4; 32]),
            exec: false,
        },
    ])
    .unwrap();

    assert_eq!(tree.entries.len(), 2);
    assert_eq!(tree.get("Foo").unwrap().id, ObjectId([3; 32]));
    assert_eq!(tree.get("foo").unwrap().id, ObjectId([4; 32]));
}
