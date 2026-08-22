use forge_core::cbor::{
    encode_array_header, encode_bool, encode_bytes, encode_map_header, encode_text, encode_u64,
    Reader,
};
use forge_core::object::encode_file;
use forge_core::{Commit, Conflict, Snapshot};
use forge_types::{ObjectId, ObjectType};

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
