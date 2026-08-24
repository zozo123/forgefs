use forge_core::ProvenanceManifest;
use forge_types::ObjectId;
use std::collections::BTreeMap;

fn manifest() -> ProvenanceManifest {
    ProvenanceManifest::new(BTreeMap::from([
        (ObjectId([0xaa; 32]), "alice".into()),
        (ObjectId([0xbb; 32]), "bob".into()),
    ]))
    .unwrap()
}

#[test]
fn provenance_manifest_is_canonical_and_roundtrips() {
    let value = manifest();
    let encoded = value.encode();
    assert_eq!(ProvenanceManifest::decode(&encoded).unwrap(), value);
    assert_eq!(value.encode(), encoded);
}

#[test]
fn provenance_manifest_rejects_noncanonical_and_malformed_keys() {
    let encoded = manifest().encode();

    let mut out_of_order = encoded.clone();
    let entry_len = 2 + 64 + 1 + "alice".len();
    let first = out_of_order[1..1 + entry_len].to_vec();
    let second = out_of_order[1 + entry_len..].to_vec();
    out_of_order.truncate(1);
    out_of_order.extend_from_slice(&second);
    out_of_order.extend_from_slice(&first);
    assert!(ProvenanceManifest::decode(&out_of_order).is_err());

    let uppercase_position = encoded[3..67]
        .iter()
        .position(|byte| *byte == b'a')
        .unwrap();
    let mut uppercase = encoded.clone();
    uppercase[uppercase_position + 3] = b'A';
    assert!(ProvenanceManifest::decode(&uppercase).is_err());

    let mut trailing = encoded;
    trailing.push(0);
    assert!(ProvenanceManifest::decode(&trailing).is_err());
}
