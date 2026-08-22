//! Canonical object encoding, hashing, and tree copy-on-write.

pub mod cbor;
pub mod contribution;
pub mod object;
pub mod tree;

pub use contribution::{Contribution, ContributionRead};
pub use object::{
    decode_object_type, hash_bytes, parse_file, Blob, Commit, Conflict, ConflictPath, Snapshot,
};
pub use tree::{apply_overlay, split_path, validate_name, Overlay, Tree, TreeEntry, TreeStore};

use forge_types::{ObjectId, Result};

pub fn put_blob_bytes(data: &[u8]) -> (ObjectId, Vec<u8>) {
    let file = Blob {
        data: data.to_vec(),
    }
    .encode();
    (hash_bytes(&file), file)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn walk_tree_entries(tree: &Tree) -> Result<Vec<(String, TreeEntry)>> {
    Ok(tree
        .entries
        .iter()
        .map(|e| (e.name.clone(), e.clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::parse_file;
    use forge_types::{EntryKind, ObjectType};

    #[test]
    fn empty_tree_roundtrip_stable_hash() {
        let t = Tree::new(vec![]).unwrap();
        let bytes = t.encode().unwrap();
        let t2 = Tree::decode(&bytes).unwrap();
        assert_eq!(t, t2);
        let id = hash_bytes(&bytes);
        assert_eq!(bytes[0], ObjectType::Tree as u8);
        // Length-prefixed file: type + u32be + header. Pin the exact bytes.
        assert_eq!(bytes.len(), 5 + 4); // empty map {e: []} is 4 cbor bytes: a1 61 65 80
        assert_eq!(&bytes[5..], &[0xa1, 0x61, b'e', 0x80]);
        let _ = id;
        let (_ty, header, payload) = parse_file(&bytes).unwrap();
        assert!(payload.is_empty());
        assert!(!header.is_empty());
        // Second encode identical.
        assert_eq!(bytes, t.encode().unwrap());
        // Malleability: longer-form uint length in the header size is a different
        // file; a non-canonical CBOR uint inside the header must be rejected.
        let noncanon = vec![0x02, 0, 0, 0, 5, 0xa1, 0x61, b'e', 0x98, 0x00];
        assert!(Tree::decode(&noncanon).is_err());
    }

    #[test]
    fn blob_roundtrip() {
        let b = Blob {
            data: b"hello forge".to_vec(),
        };
        let bytes = b.encode();
        let b2 = Blob::decode(&bytes).unwrap();
        assert_eq!(b, b2);
        assert_eq!(hash_bytes(&bytes), hash_bytes(&b.encode()));
    }

    #[test]
    fn commit_roundtrip() {
        let tree = hash_bytes(&[1, 2, 3]);
        let c = Commit {
            tree,
            parents: vec![],
            agent: "init".into(),
            msg: "init".into(),
            ts: 1,
            landmark: true,
            contrib: None,
        };
        let bytes = c.encode();
        assert_eq!(Commit::decode(&bytes).unwrap(), c);
    }

    #[test]
    fn tree_with_blob_sorted() {
        let e1 = TreeEntry {
            name: "b".into(),
            kind: EntryKind::Blob,
            id: ObjectId([1; 32]),
            exec: false,
        };
        let e2 = TreeEntry {
            name: "a".into(),
            kind: EntryKind::Blob,
            id: ObjectId([2; 32]),
            exec: true,
        };
        let t = Tree::new(vec![e1, e2]).unwrap();
        assert_eq!(t.entries[0].name, "a");
        let bytes = t.encode().unwrap();
        let t2 = Tree::decode(&bytes).unwrap();
        assert_eq!(t, t2);
    }

    #[test]
    fn snapshot_unsigned_differs_by_sig_only_in_bytes() {
        let s = Snapshot {
            tree: ObjectId([9; 32]),
            commit: ObjectId([8; 32]),
            tag: "v1.0".into(),
            ts: 42,
            prov: ObjectId([7; 32]),
            pk: [3; 32],
            sig: [4; 64],
        };
        let u = s.encode_unsigned();
        let s2 = Snapshot::decode(&u).unwrap();
        assert_eq!(s2.sig, [0u8; 64]);
        assert_eq!(s2.tag, "v1.0");
    }
}
