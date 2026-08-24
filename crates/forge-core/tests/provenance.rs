use forge_core::cbor::{encode_map_sorted, encode_text, encode_u64, text_key};
use forge_core::ProvenanceManifest;
use forge_types::ObjectId;
use std::collections::BTreeMap;

fn entries() -> BTreeMap<ObjectId, String> {
    BTreeMap::from([
        (ObjectId([0xaa; 32]), "alice".into()),
        (ObjectId([0xbb; 32]), "bob".into()),
    ])
}

fn manifest() -> ProvenanceManifest {
    ProvenanceManifest::new(entries()).unwrap()
}

fn legacy_encoded() -> Vec<u8> {
    let pairs = entries()
        .into_iter()
        .map(|(id, agent)| {
            let mut key = Vec::new();
            encode_text(&mut key, &id.hex());
            let mut value = Vec::new();
            encode_text(&mut value, &agent);
            (key, value)
        })
        .collect();
    let mut encoded = Vec::new();
    encode_map_sorted(&mut encoded, pairs);
    encoded
}

#[test]
fn provenance_manifest_is_canonical_and_roundtrips() {
    let value = manifest();
    let encoded = value.encode();
    assert_eq!(ProvenanceManifest::decode(&encoded).unwrap(), value);
    assert_eq!(value.encode(), encoded);
    assert!(!value.is_legacy());

    let legacy = ProvenanceManifest::decode(&legacy_encoded()).unwrap();
    assert!(legacy.is_legacy());
    assert_eq!(legacy.entries(), &entries());
}

#[test]
fn provenance_manifest_rejects_noncanonical_and_malformed_keys() {
    let encoded = legacy_encoded();

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

#[test]
fn provenance_manifest_rejects_unknown_envelope_versions() {
    let mut version = Vec::new();
    encode_u64(&mut version, 2);
    let mut encoded = Vec::new();
    encode_map_sorted(
        &mut encoded,
        vec![
            (text_key("entries"), legacy_encoded()),
            (text_key("version"), version),
        ],
    );

    let error = ProvenanceManifest::decode(&encoded).unwrap_err();
    assert!(error.to_string().contains("unsupported provenance version 2"));
}
