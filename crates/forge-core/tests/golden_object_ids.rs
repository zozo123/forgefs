use forge_core::{hash_bytes, Blob, Commit, Conflict, ConflictPath, Snapshot, Tree};
use forge_types::ObjectId;

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
        hash_bytes(&commit.encode()).hex(),
        expected(include_str!("../../../testdata/canonical/commit.oid"))
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
