use forge_core::{hash_bytes, Blob, Commit, Conflict, ConflictPath, Snapshot, Tree};
use forge_types::{hex_decode, ObjectId, ObjectType};

fn expected(s: &str) -> &str {
    s.trim()
}

#[test]
fn v1_object_ids_are_userspace_abi() {
    // These hashes are compatibility vectors, not snapshots to regenerate casually.
    let blob = Blob {
        data: b"forgefs-v1-golden".to_vec(),
    };
    let tree = Tree::new(vec![]).unwrap();
    let commit = Commit {
        tree: ObjectId([0x22; 32]),
        parents: vec![ObjectId([0x33; 32]), ObjectId([0x44; 32])],
        agent: "golden".into(),
        msg: "v1".into(),
        ts: 42,
        landmark: true,
        contrib: None,
    };
    let commit_bytes =
        hex_decode(include_str!("../../../testdata/canonical/commit.hex").trim()).unwrap();
    assert_eq!(commit.encode(), commit_bytes);
    assert_eq!(Commit::decode(&commit_bytes).unwrap(), commit);

    let commit_with_contribution = Commit {
        contrib: Some(ObjectId([0x55; 32])),
        ..commit.clone()
    };
    let commit_with_contribution_bytes =
        hex_decode(include_str!("../../../testdata/canonical/commit_with_contribution.hex").trim())
            .unwrap();
    assert_eq!(
        commit_with_contribution.encode(),
        commit_with_contribution_bytes
    );
    assert_eq!(
        Commit::decode(&commit_with_contribution_bytes).unwrap(),
        commit_with_contribution
    );
    let conflict = Conflict {
        bases: vec![ObjectId([0x55; 32])],
        ours: ObjectId([0x66; 32]),
        theirs: ObjectId([0x77; 32]),
        paths: vec![ConflictPath {
            path: "same.txt".into(),
            a: Some(ObjectId([0x88; 32])),
            b: None,
            base: Some(ObjectId([0x99; 32])),
        }],
        causal: vec![ObjectId([0xaa; 32]), ObjectId([0xbb; 32])],
    };
    let snapshot = Snapshot {
        tree: ObjectId([0xcc; 32]),
        commit: ObjectId([0xdd; 32]),
        tag: "v1.0".into(),
        ts: 43,
        prov: ObjectId([0xee; 32]),
        pk: [0x12; 32],
        sig: [0x34; 64],
    };

    assert_eq!(
        hash_bytes(&blob.encode()).hex(),
        expected(include_str!("../../../testdata/canonical/blob.oid"))
    );
    assert_eq!(
        hash_bytes(&tree.encode().unwrap()).hex(),
        expected(include_str!("../../../testdata/canonical/empty_tree.oid"))
    );
    assert_eq!(
        hash_bytes(&commit_bytes).hex(),
        expected(include_str!("../../../testdata/canonical/commit.oid"))
    );
    assert_eq!(
        hash_bytes(&commit_with_contribution_bytes).hex(),
        expected(include_str!(
            "../../../testdata/canonical/commit_with_contribution.oid"
        ))
    );
    assert_ne!(
        hash_bytes(&commit_bytes),
        hash_bytes(&commit_with_contribution_bytes),
        "the optional Contribution edge participates in Commit identity"
    );
    assert_eq!(
        hash_bytes(&conflict.encode()).hex(),
        expected(include_str!("../../../testdata/canonical/conflict.oid"))
    );
    assert_eq!(
        hash_bytes(&snapshot.encode()).hex(),
        expected(include_str!("../../../testdata/canonical/snapshot.oid"))
    );
}

#[test]
fn v1_object_type_registry_is_frozen() {
    let assigned = [
        (0x01, ObjectType::Blob),
        (0x02, ObjectType::Tree),
        (0x03, ObjectType::Commit),
        (0x04, ObjectType::Conflict),
        (0x05, ObjectType::Snapshot),
        (0x06, ObjectType::Contribution),
    ];
    for &(tag, expected) in &assigned {
        assert_eq!(expected as u8, tag);
        assert_eq!(ObjectType::from_u8(tag).unwrap(), expected);
    }

    for unassigned in u8::MIN..=u8::MAX {
        if assigned.iter().any(|(tag, _)| *tag == unassigned) {
            continue;
        }
        assert!(
            ObjectType::from_u8(unassigned).is_err(),
            "VERSION 1 must reject unassigned type 0x{unassigned:02x}"
        );
    }
}
