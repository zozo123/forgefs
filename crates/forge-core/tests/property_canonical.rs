//! I1/I2 as executable algebra.
//!
//! I1: decode(encode(x)) is encode(x) -- decoding canonical bytes is the
//! identity on the logical object, and re-encoding reproduces the same bytes.
//! I2: one logical object => one byte string => one ObjectId -- the encoding
//! cannot depend on incidental input order.
//!
//! Deterministic and dependency-free: a SplitMix64 seed drives generation, so
//! every failure names the exact seed that produced it and reproduces on the
//! next run. The repository deliberately has no property-testing crate; this
//! keeps it that way.

use forge_core::{hash_bytes, Blob, Commit, Conflict, ConflictPath, Snapshot, Tree, TreeEntry};
use forge_types::{EntryKind, ObjectId};

const CASES: u64 = 400;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    fn flag(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    fn oid(&mut self) -> ObjectId {
        let mut bytes = [0u8; 32];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        ObjectId(bytes)
    }

    /// A deliberately small alphabet with multi-byte and boundary code points,
    /// so name collisions and rejection paths are actually reached.
    fn text(&mut self, max: usize) -> String {
        const ALPHABET: [char; 10] = ['a', 'b', 'Z', '0', '-', '_', ' ', 'é', '中', '~'];
        let n = 1 + self.below(max);
        (0..n)
            .map(|_| ALPHABET[self.below(ALPHABET.len())])
            .collect()
    }

    fn entries(&mut self, max: usize) -> Vec<TreeEntry> {
        let n = self.below(max);
        (0..n)
            .map(|_| TreeEntry {
                name: self.text(6),
                kind: if self.flag() {
                    EntryKind::Blob
                } else {
                    EntryKind::Tree
                },
                id: self.oid(),
                exec: self.flag(),
            })
            .collect()
    }
}

#[test]
fn blob_decode_is_the_identity_on_canonical_bytes_i1() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_b10b_0000);
        let n = rng.below(2048);
        let data: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
        let value = Blob { data };
        let bytes = value.encode();
        let back = Blob::decode(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: canonical blob failed to decode: {e:?}"));
        assert_eq!(back, value, "seed {seed}: blob decode is not the identity");
        assert_eq!(
            back.encode(),
            bytes,
            "seed {seed}: re-encoding a decoded blob changed its canonical bytes"
        );
    }
}

#[test]
fn tree_decode_is_the_identity_on_canonical_bytes_i1() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_7ee0_0000);
        let Ok(tree) = Tree::new(rng.entries(12)) else {
            continue; // duplicate generated names are a legitimate rejection
        };
        let bytes = tree.encode().expect("valid tree encodes");
        let back = Tree::decode(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: canonical tree failed to decode: {e:?}"));
        assert_eq!(back, tree, "seed {seed}: tree decode is not the identity");
        assert_eq!(
            back.encode().expect("decoded tree re-encodes"),
            bytes,
            "seed {seed}: re-encoding a decoded tree changed its canonical bytes"
        );
    }
}

#[test]
fn tree_bytes_do_not_depend_on_input_entry_order_i2() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_0d0e_0000);
        let entries = rng.entries(12);
        let Ok(tree) = Tree::new(entries.clone()) else {
            continue;
        };
        let bytes = tree.encode().expect("valid tree encodes");
        let id = hash_bytes(&bytes);

        // Same logical tree, different incidental construction order.
        let mut shuffled = entries;
        let len = shuffled.len();
        for i in (1..len).rev() {
            shuffled.swap(i, rng.below(i + 1));
        }
        let other = Tree::new(shuffled).expect("a permutation of a valid tree is valid");
        let other_bytes = other.encode().expect("valid tree encodes");

        assert_eq!(
            other_bytes, bytes,
            "seed {seed}: tree encoding depends on input entry order (I2)"
        );
        assert_eq!(
            hash_bytes(&other_bytes),
            id,
            "seed {seed}: one logical tree produced two ObjectIds (I2)"
        );
    }
}

#[test]
fn commit_decode_is_the_identity_on_canonical_bytes_i1() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_c0dd_0000);
        let parents = (0..rng.below(4)).map(|_| rng.oid()).collect();
        let value = Commit {
            tree: rng.oid(),
            parents,
            agent: rng.text(8),
            msg: rng.text(24),
            ts: rng.next_u64(),
            landmark: rng.flag(),
            contrib: if rng.flag() { Some(rng.oid()) } else { None },
        };
        let bytes = value.encode();
        let back = Commit::decode(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: canonical commit failed to decode: {e:?}"));
        assert_eq!(
            back, value,
            "seed {seed}: commit decode is not the identity"
        );
        assert_eq!(
            back.encode(),
            bytes,
            "seed {seed}: re-encoding a decoded commit changed its canonical bytes"
        );
        assert_eq!(
            hash_bytes(&back.encode()),
            hash_bytes(&bytes),
            "seed {seed}: one logical commit produced two ObjectIds (I2)"
        );
    }
}

#[test]
fn conflict_decode_is_the_identity_on_canonical_bytes_i1() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_c0f0_0000);
        let paths = (0..rng.below(6))
            .map(|_| ConflictPath {
                path: rng.text(12),
                a: if rng.flag() { Some(rng.oid()) } else { None },
                b: if rng.flag() { Some(rng.oid()) } else { None },
                base: if rng.flag() { Some(rng.oid()) } else { None },
            })
            .collect();
        let value = Conflict {
            bases: (0..rng.below(3)).map(|_| rng.oid()).collect(),
            ours: rng.oid(),
            theirs: rng.oid(),
            paths,
            causal: (0..rng.below(3)).map(|_| rng.oid()).collect(),
        };
        let bytes = value.encode();
        let back = Conflict::decode(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: canonical conflict failed to decode: {e:?}"));
        assert_eq!(
            back, value,
            "seed {seed}: conflict decode is not the identity"
        );
        assert_eq!(
            back.encode(),
            bytes,
            "seed {seed}: re-encoding a decoded conflict changed its canonical bytes"
        );
    }
}

#[test]
fn snapshot_decode_is_the_identity_on_canonical_bytes_i1() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_5a0f_0000);
        let mut pk = [0u8; 32];
        for b in pk.iter_mut() {
            *b = rng.next_u64() as u8;
        }
        let mut sig = [0u8; 64];
        for b in sig.iter_mut() {
            *b = rng.next_u64() as u8;
        }
        let value = Snapshot {
            tree: rng.oid(),
            commit: rng.oid(),
            tag: rng.text(10),
            ts: rng.next_u64(),
            prov: rng.oid(),
            pk,
            sig,
        };
        let bytes = value.encode();
        let back = Snapshot::decode(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: canonical snapshot failed to decode: {e:?}"));
        assert_eq!(
            back, value,
            "seed {seed}: snapshot decode is not the identity"
        );
        assert_eq!(
            back.encode(),
            bytes,
            "seed {seed}: re-encoding a decoded snapshot changed its canonical bytes"
        );

        // The signed and unsigned encodings differ only in the signature field,
        // and the unsigned form must be a fixed function of the same object.
        let unsigned = value.encode_unsigned();
        let mut zeroed = value.clone();
        zeroed.sig = [0u8; 64];
        assert_eq!(
            Snapshot::decode(&unsigned).expect("unsigned snapshot decodes"),
            zeroed,
            "seed {seed}: unsigned snapshot encoding is not the object minus its signature"
        );
    }
}
