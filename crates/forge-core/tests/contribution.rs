use forge_core::{hash_bytes, Blob, Contribution, ContributionRead};
use forge_types::ObjectId;

fn sample() -> Contribution {
    Contribution {
        base: ObjectId([0x10; 32]),
        tree: ObjectId([0x20; 32]),
        parents: vec![ObjectId([0x30; 32])],
        reads: vec![
            ContributionRead {
                path: "Cargo.lock".into(),
                id: ObjectId([0x40; 32]),
            },
            ContributionRead {
                path: "src/lib.rs".into(),
                id: ObjectId([0x41; 32]),
            },
        ],
        writes: vec!["README.md".into(), "src/lib.rs".into()],
        agent: "agent-a".into(),
        ts: 1,
    }
}

#[test]
fn contribution_roundtrip_and_golden_identity() {
    let value = sample();
    let bytes = value.encode().unwrap();
    assert_eq!(Contribution::decode(&bytes).unwrap(), value);
    assert_eq!(
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        include_str!("../../../testdata/canonical/contribution.hex").trim()
    );
    assert_eq!(
        hash_bytes(&bytes).hex(),
        include_str!("../../../testdata/canonical/contribution.oid").trim()
    );
}

#[test]
fn contribution_requires_bytewise_sorted_unique_sets() {
    let mut value = sample();
    value.writes.reverse();
    assert!(value.encode().is_err());

    let mut value = sample();
    value.reads[1].path = value.reads[0].path.clone();
    assert!(value.encode().is_err());
}

#[test]
fn contribution_decoder_is_type_strict() {
    let blob = Blob {
        data: b"x".to_vec(),
    }
    .encode();
    assert!(Contribution::decode(&blob).is_err());
}

#[test]
fn commit_contribution_link_roundtrips() {
    use forge_core::Commit;

    let contrib = ObjectId([9; 32]);
    let commit = Commit {
        tree: ObjectId([1; 32]),
        parents: vec![ObjectId([2; 32])],
        agent: "agent".into(),
        msg: "msg".into(),
        ts: 7,
        landmark: false,
        contrib: Some(contrib),
    };
    let decoded = Commit::decode(&commit.encode()).unwrap();
    assert_eq!(decoded.contrib, Some(contrib));
}
