use forge_core::cbor::{
    encode_array_header, encode_bool, encode_bytes, encode_map_header, encode_text, encode_u64,
    Reader,
};
use forge_core::object::encode_file;
use forge_core::{
    parse_file, Blob, Commit, Conflict, Contribution, ContributionRead, Snapshot, Tree, TreeEntry,
};
use forge_types::{EntryKind, Error, ObjectId, ObjectType};

fn append_kv(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    encode_text(out, key);
    out.extend_from_slice(value);
}

#[test]
fn commit_rejects_unsorted_map_keys() {
    let mut header = Vec::new();
    encode_map_header(&mut header, 6);

    let mut agent = Vec::new();
    encode_text(&mut agent, "agent");
    let mut lm = Vec::new();
    encode_bool(&mut lm, false);
    let mut msg = Vec::new();
    encode_text(&mut msg, "msg");
    let mut parents = Vec::new();
    encode_array_header(&mut parents, 0);
    let mut tree = Vec::new();
    encode_bytes(&mut tree, ObjectId([1; 32]).as_bytes());
    let mut ts = Vec::new();
    encode_u64(&mut ts, 1);

    // Deliberately not canonical CBOR encoded-key order: `agent` (0x65...)
    // precedes shorter keys such as `lm` (0x62...).
    append_kv(&mut header, "agent", &agent);
    append_kv(&mut header, "lm", &lm);
    append_kv(&mut header, "msg", &msg);
    append_kv(&mut header, "parents", &parents);
    append_kv(&mut header, "tree", &tree);
    append_kv(&mut header, "ts", &ts);

    let bytes = encode_file(ObjectType::Commit, &header, &[]);
    assert!(Commit::decode(&bytes).is_err());
}

#[test]
fn commit_rejects_trailing_header_bytes() {
    let c = Commit {
        tree: ObjectId([1; 32]),
        parents: vec![],
        agent: "a".into(),
        msg: "m".into(),
        ts: 1,
        landmark: false,
        contrib: None,
    };
    let mut bytes = c.encode();
    let n = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    bytes.insert(5 + n, 0xf6);
    let new_n = (n + 1) as u32;
    bytes[1..5].copy_from_slice(&new_n.to_be_bytes());
    assert!(Commit::decode(&bytes).is_err());
}

#[test]
fn conflict_and_snapshot_reject_payloads() {
    let c = Conflict {
        bases: vec![],
        ours: ObjectId([1; 32]),
        theirs: ObjectId([2; 32]),
        paths: vec![],
        causal: vec![],
    };
    let mut conflict = c.encode();
    conflict.push(0);
    assert!(Conflict::decode(&conflict).is_err());

    let s = Snapshot {
        tree: ObjectId([1; 32]),
        commit: ObjectId([2; 32]),
        tag: "t".into(),
        ts: 1,
        prov: ObjectId([3; 32]),
        pk: [4; 32],
        sig: [5; 64],
    };
    let mut snapshot = s.encode();
    snapshot.push(0);
    assert!(Snapshot::decode(&snapshot).is_err());
}

#[test]
fn huge_declared_length_returns_error_without_panicking() {
    let bytes = [0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    let outcome = std::panic::catch_unwind(|| {
        let mut r = Reader::new(&bytes);
        r.bytes()
    });
    assert!(
        matches!(outcome, Ok(Err(_))),
        "decoder panicked or accepted impossible length"
    );
}

/// I1: unknown header fields are Corrupt. A decoder that skipped an unknown key
/// would accept bytes it cannot reproduce, so `Decode(encode(x)) == encode(x)`
/// would no longer hold and two distinct byte strings would name one object.
#[test]
fn i1_decoders_reject_unknown_header_fields() {
    // Canonical map order is by encoded key bytes, so a 24-byte text key
    // (0x78 0x18 ...) sorts after every real key in the v1 format. The only
    // non-canonical property of a spliced object is the unknown field itself.
    const UNKNOWN_KEY: &str = "zzzzzzzzzzzzzzzzzzzzzzzz";

    fn with_unknown_field(bytes: &[u8]) -> Vec<u8> {
        let (ty, header, payload) = parse_file(bytes).expect("valid object parses");
        assert_eq!(
            header[0] & 0xe0,
            0xa0,
            "object header must start with a CBOR map"
        );
        let count = header[0] & 0x1f;
        assert!(count < 23, "map count must stay in the one-byte form");
        let mut grown = header.to_vec();
        grown[0] = 0xa0 | (count + 1);
        encode_text(&mut grown, UNKNOWN_KEY);
        encode_text(&mut grown, "ignored");
        encode_file(ty, &grown, payload)
    }

    fn assert_corrupt(what: &str, decoded: Result<(), Error>) {
        match decoded {
            Err(Error::Corrupt(_)) => {}
            Err(other) => panic!("{what} rejected an unknown field as {other:?}, not Corrupt"),
            Ok(()) => panic!("{what} decoder accepted an unknown header field"),
        }
    }

    let blob = Blob {
        data: b"forgefs-unknown-field".to_vec(),
    }
    .encode();
    assert_corrupt("blob", Blob::decode(&with_unknown_field(&blob)).map(|_| ()));

    let tree = Tree::new(vec![TreeEntry {
        name: "alpha".into(),
        kind: EntryKind::Blob,
        id: ObjectId([0x11; 32]),
        exec: false,
    }])
    .unwrap()
    .encode()
    .unwrap();
    assert_corrupt("tree", Tree::decode(&with_unknown_field(&tree)).map(|_| ()));

    let commit = Commit {
        tree: ObjectId([0x33; 32]),
        parents: vec![ObjectId([0x44; 32])],
        agent: "agent-a".into(),
        msg: "unknown field must not be tolerated".into(),
        ts: 42,
        landmark: false,
        contrib: None,
    }
    .encode();
    assert_corrupt(
        "commit",
        Commit::decode(&with_unknown_field(&commit)).map(|_| ()),
    );

    let conflict = Conflict {
        bases: vec![ObjectId([1; 32])],
        ours: ObjectId([2; 32]),
        theirs: ObjectId([3; 32]),
        paths: vec![],
        causal: vec![],
    }
    .encode();
    assert_corrupt(
        "conflict",
        Conflict::decode(&with_unknown_field(&conflict)).map(|_| ()),
    );

    let snapshot = Snapshot {
        tree: ObjectId([1; 32]),
        commit: ObjectId([2; 32]),
        tag: "v0".into(),
        ts: 1,
        prov: ObjectId([3; 32]),
        pk: [4; 32],
        sig: [5; 64],
    }
    .encode();
    assert_corrupt(
        "snapshot",
        Snapshot::decode(&with_unknown_field(&snapshot)).map(|_| ()),
    );

    let contribution = Contribution {
        base: ObjectId([0x10; 32]),
        tree: ObjectId([0x20; 32]),
        parents: vec![ObjectId([0x30; 32])],
        reads: vec![ContributionRead {
            path: "src/lib.rs".into(),
            id: ObjectId([0x40; 32]),
        }],
        writes: vec!["README.md".into()],
        agent: "agent-a".into(),
        ts: 1,
    }
    .encode()
    .unwrap();
    assert_corrupt(
        "contribution",
        Contribution::decode(&with_unknown_field(&contribution)).map(|_| ()),
    );
}
