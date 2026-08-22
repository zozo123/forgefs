use forge_core::{Blob, Commit, Tree, TreeEntry};
use forge_types::{EntryKind, ObjectId};

fn mutations(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();

    // Every truncation is a useful parser boundary case.
    for end in 0..bytes.len() {
        out.push(bytes[..end].to_vec());
    }

    // Flip every bit of every byte. Bound the corpus while still covering the
    // complete valid encoding surface of these small golden objects.
    for i in 0..bytes.len() {
        for bit in 0..8 {
            let mut mutated = bytes.to_vec();
            mutated[i] ^= 1 << bit;
            out.push(mutated);
        }
    }

    // Exercise trailing-data rejection explicitly.
    let mut trailing = bytes.to_vec();
    trailing.extend_from_slice(&[0, 0xff, 0x80]);
    out.push(trailing);
    out
}

#[test]
fn accepted_blob_mutations_are_canonical() {
    let original = Blob {
        data: b"forgefs-canonical-blob".to_vec(),
    }
    .encode();

    for candidate in std::iter::once(original.clone()).chain(mutations(&original)) {
        if let Ok(decoded) = Blob::decode(&candidate) {
            assert_eq!(
                decoded.encode(),
                candidate,
                "blob decoder accepted a non-canonical representation"
            );
        }
    }
}

#[test]
fn accepted_tree_mutations_are_canonical() {
    let original = Tree::new(vec![
        TreeEntry {
            name: "alpha".into(),
            kind: EntryKind::Blob,
            id: ObjectId([0x11; 32]),
            exec: false,
        },
        TreeEntry {
            name: "omega".into(),
            kind: EntryKind::Tree,
            id: ObjectId([0x22; 32]),
            exec: false,
        },
    ])
    .unwrap()
    .encode()
    .unwrap();

    for candidate in std::iter::once(original.clone()).chain(mutations(&original)) {
        if let Ok(decoded) = Tree::decode(&candidate) {
            assert_eq!(
                decoded.encode().unwrap(),
                candidate,
                "tree decoder accepted a non-canonical representation"
            );
        }
    }
}

#[test]
fn accepted_commit_mutations_are_canonical() {
    let original = Commit {
        tree: ObjectId([0x33; 32]),
        parents: vec![ObjectId([0x44; 32]), ObjectId([0x55; 32])],
        agent: "agent-a".into(),
        msg: "deterministic mutation corpus".into(),
        ts: 42,
        landmark: true,
        contrib: None,
    }
    .encode();

    for candidate in std::iter::once(original.clone()).chain(mutations(&original)) {
        if let Ok(decoded) = Commit::decode(&candidate) {
            assert_eq!(
                decoded.encode(),
                candidate,
                "commit decoder accepted a non-canonical representation"
            );
        }
    }
}
