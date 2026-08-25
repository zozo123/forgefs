//! #355: over-fanout a CALLER supplied is `Invalid` (exit 1); over-fanout read
//! back OUT of the store is `Corrupt` (exit 2).
//!
//! `MAX_TREE_ENTRIES` used to be enforced only in `Tree::decode`, so a
//! directory of more than 100_000 entries was written happily and then
//! discovered on the way back out, as `corrupt: tree fanout exceeds limit`,
//! exit 2. Exit 2 is reserved for a damaged repository (issue #348); a large
//! directory is caller input and nothing on disk is wrong. The codebase already
//! disagreed with itself about it: `Contribution::validate` had raised
//! `Invalid` for the identical limit since it was written.
//!
//! Both directions are asserted here on purpose. Collapsing the pair into
//! `Invalid` would lose a real corruption signal -- an object file that decodes
//! to a fanout no encoder in this binary can produce IS damage -- and
//! collapsing it into `Corrupt` is the defect. So each test names an exit code
//! the other test forbids.

use forge_core::cbor::{
    encode_array_header, encode_bool, encode_bytes, encode_map_sorted, encode_text, encode_u64,
    text_key,
};
use forge_core::object::encode_file;
use forge_core::{Conflict, ConflictPath, Tree, TreeEntry, MAX_CONFLICT_ITEMS, MAX_TREE_ENTRIES};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId, ObjectType};
use tempfile::tempdir;

/// Names that are already bytewise sorted and unique, so the bytes below are
/// canonical in every respect EXCEPT their entry count.
fn name(i: u64) -> String {
    format!("f{i:07}")
}

fn entries(n: u64) -> Vec<TreeEntry> {
    (0..n)
        .map(|i| TreeEntry {
            name: name(i),
            kind: EntryKind::Blob,
            id: ObjectId::ZERO,
            exec: false,
        })
        .collect()
}

/// A canonical VERSION 1 tree file with `n` entries, built WITHOUT
/// `Tree::encode`.
///
/// It has to be built by hand: the encoder now refuses this fanout, which is
/// the whole point of `caller_supplied_over_fanout_is_invalid_not_corrupt`. A
/// tree file of this shape can therefore no longer originate from any caller
/// path, which is exactly why finding one in the store is damage.
fn oversized_tree_file(n: u64) -> Vec<u8> {
    let mut entries_cbor = Vec::new();
    encode_array_header(&mut entries_cbor, n);
    for i in 0..n {
        let mut n_v = Vec::new();
        encode_text(&mut n_v, &name(i));
        let mut k_v = Vec::new();
        encode_u64(&mut k_v, EntryKind::Blob as u64);
        let mut id_v = Vec::new();
        encode_bytes(&mut id_v, ObjectId::ZERO.as_bytes());
        let mut x_v = Vec::new();
        encode_bool(&mut x_v, false);
        encode_map_sorted(
            &mut entries_cbor,
            vec![
                (text_key("id"), id_v),
                (text_key("k"), k_v),
                (text_key("n"), n_v),
                (text_key("x"), x_v),
            ],
        );
    }
    let mut header = Vec::new();
    encode_map_sorted(&mut header, vec![(text_key("e"), entries_cbor)]);
    encode_file(ObjectType::Tree, &header, &[])
}

#[test]
fn caller_supplied_over_fanout_is_invalid_not_corrupt() {
    let tree = Tree::new(entries(MAX_TREE_ENTRIES + 1)).expect("names are legal and unique");
    // Not `expect_err`: on the failing tree the Ok value is megabytes of CBOR
    // and Debug-printing it buries the diagnostic.
    let Err(err) = tree.encode() else {
        panic!(
            "the encoder produced {} entries of tree bytes; nothing then stops them reaching the \
             store, where Tree::decode calls the same fanout Corrupt (exit 2) on a repository \
             that is not damaged (#355)",
            MAX_TREE_ENTRIES + 1
        )
    };
    assert_eq!(
        err.exit_code(),
        1,
        "entries a caller handed us are INPUT: exit 1. Exit 2 means the repository is corrupt \
         and this one is untouched (#348, #355). Got: {err}"
    );
    assert!(
        matches!(err, Error::Invalid(_)),
        "the caller-input side of MAX_TREE_ENTRIES must be Invalid, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains(&MAX_TREE_ENTRIES.to_string()) && msg.contains("100001"),
        "the refusal must name the limit and the size that broke it: {msg}"
    );
}

#[test]
fn a_tree_at_the_limit_still_encodes() {
    let tree = Tree::new(entries(MAX_TREE_ENTRIES)).unwrap();
    let bytes = tree.encode().expect("exactly the limit is legal");
    assert_eq!(
        Tree::decode(&bytes).unwrap().entries.len() as u64,
        MAX_TREE_ENTRIES,
        "the boundary must be > and not >=, in both directions"
    );
}

#[test]
fn an_over_fanout_tree_read_back_from_the_store_is_still_corrupt() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();

    // Plant the damage the encoder can no longer produce, then read it back the
    // way every consumer does.
    let id = store
        .put_raw(&oversized_tree_file(MAX_TREE_ENTRIES + 1))
        .unwrap();
    let Err(err) = store.get_tree(id) else {
        panic!("a tree object with an impossible fanout must not decode")
    };
    assert_eq!(
        err.exit_code(),
        2,
        "bytes IN THE STORE that no encoder could have written are damage: exit 2. Answering 1 \
         here would lose the corruption signal #355 must keep. Got: {err}"
    );
    assert!(
        matches!(err, Error::Corrupt(_)),
        "the read-back side of MAX_TREE_ENTRIES must stay Corrupt, got {err:?}"
    );
}

#[test]
fn an_over_fanout_conflict_is_input_on_the_way_in_and_corrupt_on_the_way_out() {
    let d = tempdir().unwrap();
    let store = Store::open(d.path()).unwrap();
    let conflict = Conflict {
        bases: Vec::new(),
        ours: ObjectId::ZERO,
        theirs: ObjectId::ZERO,
        paths: (0..=MAX_CONFLICT_ITEMS)
            .map(|i| ConflictPath {
                path: name(i),
                a: None,
                b: None,
                base: None,
            })
            .collect(),
        causal: Vec::new(),
    };

    // `Conflict::encode` cannot fail, so the ONLY gate between a merge a caller
    // asked for and bytes that will not decode is `Store::put_conflict`.
    let Err(err) = store.put_conflict(&conflict) else {
        panic!(
            "a conflict of {} paths was stored; Conflict::decode calls that fanout Corrupt \
             (exit 2), so a merge a caller asked for manufactures a corruption report (#355)",
            MAX_CONFLICT_ITEMS + 1
        )
    };
    assert_eq!(
        err.exit_code(),
        1,
        "a merge with too many conflicting paths is the caller's request, not damage: {err}"
    );
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");

    // And the same bytes, if they ever reach the store, are still corruption.
    let Err(err) = Conflict::decode(&conflict.encode()) else {
        panic!("the decoder must still refuse this fanout")
    };
    assert_eq!(
        err.exit_code(),
        2,
        "the read-back side of MAX_CONFLICT_ITEMS must stay Corrupt: {err}"
    );
}
