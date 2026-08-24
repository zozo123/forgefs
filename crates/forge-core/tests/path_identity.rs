use forge_core::{split_path, validate_name, Tree, TreeEntry};
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

/// I16: a tree name is exactly one path component, held as its exact UTF-8
/// bytes. A name carrying the path separator or a NUL byte would let two
/// byte-distinct trees claim the same resolved path, so core identity must
/// reject such a name outright rather than reshape it.
#[test]
fn i16_tree_name_rejects_path_separator_and_nul() {
    for illegal in ["a/b", "/", "dir/", "nul\0byte", "\0"] {
        assert!(
            validate_name(illegal).is_err(),
            "validate_name must reject the tree name {illegal:?}"
        );

        let entry = TreeEntry {
            name: illegal.into(),
            kind: EntryKind::Blob,
            id: ObjectId([5; 32]),
            exec: false,
        };
        assert!(
            Tree::new(vec![entry.clone()]).is_err(),
            "Tree::new must reject the tree name {illegal:?}"
        );
        assert!(
            Tree::from_canonical(vec![entry]).is_err(),
            "decode must reject the tree name {illegal:?}"
        );
    }

    assert!(
        split_path("dir/nul\0byte").is_err(),
        "a NUL byte must not survive path splitting into a tree name"
    );
}
