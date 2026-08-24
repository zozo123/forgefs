#![no_main]

use forge_core::{validate_name, Tree, TreeEntry};
use forge_types::{EntryKind, ObjectId};
use libfuzzer_sys::fuzz_target;

// Tree names arrive from untrusted hosts (import) and untrusted peers
// (decode). The grammar must fail closed, never panic, and every name it
// accepts must survive canonical encode/decode unchanged (I1/I2).
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    let mut accepted: Vec<&str> = Vec::new();
    for name in text.split('\n').take(64) {
        if validate_name(name).is_err() {
            continue;
        }
        // An accepted name never carries a path separator, a NUL, a dot-dot
        // escape, or an out-of-range length. Restating the grammar here means a
        // future loosening of `validate_name` fails the fuzzer instead of
        // silently widening what import and decode will accept.
        assert!(
            !name.is_empty() && name.len() <= 255,
            "accepted out-of-range tree name {name:?}"
        );
        assert!(
            !name.contains('/') && !name.contains('\0'),
            "accepted separator in tree name {name:?}"
        );
        assert!(name != "." && name != "..", "accepted dot name {name:?}");
        accepted.push(name);
    }

    let entries: Vec<TreeEntry> = accepted
        .iter()
        .enumerate()
        .map(|(i, name)| TreeEntry {
            name: (*name).to_string(),
            kind: if i % 2 == 0 {
                EntryKind::Blob
            } else {
                EntryKind::Tree
            },
            id: ObjectId([i as u8; 32]),
            exec: i % 3 == 0,
        })
        .collect();

    // Duplicate names are a legitimate rejection, not a bug.
    let Ok(tree) = Tree::new(entries) else {
        return;
    };
    let Ok(bytes) = tree.encode() else {
        return;
    };
    let decoded = Tree::decode(&bytes).expect("canonical tree bytes must decode");
    assert_eq!(
        decoded, tree,
        "tree decode is not the identity on canonical bytes"
    );
    assert_eq!(
        decoded.encode().expect("decoded tree re-encodes"),
        bytes,
        "re-encoding a decoded tree changed its canonical bytes"
    );
});
