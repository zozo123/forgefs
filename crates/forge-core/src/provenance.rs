//! Canonical payload stored in a Snapshot's provenance Blob.

use crate::cbor::{encode_map_sorted, encode_text, Reader};
use forge_types::{Error, ObjectId, Result};
use std::collections::BTreeMap;

const MAX_ENTRIES: u64 = 1_000_000;
const MAX_LABEL_BYTES: usize = 1_024;
const MAX_SERIALIZED_BYTES: usize = 64 * 1024 * 1024;

fn cbor_header_len(value: usize) -> usize {
    match value {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

fn encoded_text_len(bytes: usize) -> Result<usize> {
    cbor_header_len(bytes)
        .checked_add(bytes)
        .ok_or_else(|| Error::Invalid("provenance size overflow".into()))
}

fn validate_entries(entries: &BTreeMap<ObjectId, String>, max_bytes: usize) -> Result<()> {
    if entries.len() > MAX_ENTRIES as usize {
        return Err(Error::Invalid("provenance entry limit".into()));
    }
    let encoded_key_len = encoded_text_len(64)?;
    let mut serialized = cbor_header_len(entries.len());
    if serialized > max_bytes {
        return Err(Error::Invalid("provenance serialized size limit".into()));
    }
    for label in entries.values() {
        if label.len() > MAX_LABEL_BYTES {
            return Err(Error::Invalid("provenance attribution label limit".into()));
        }
        let encoded_label_len = encoded_text_len(label.len())?;
        serialized = serialized
            .checked_add(encoded_key_len)
            .and_then(|size| size.checked_add(encoded_label_len))
            .ok_or_else(|| Error::Invalid("provenance size overflow".into()))?;
        if serialized > max_bytes {
            return Err(Error::Invalid("provenance serialized size limit".into()));
        }
    }
    Ok(())
}

/// The signed provenance index for a sealed snapshot.
///
/// Keys are canonical lowercase ObjectId hex and values are attribution
/// labels. The snapshot signs the Blob ObjectId containing these bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceManifest {
    entries: BTreeMap<ObjectId, String>,
}

impl ProvenanceManifest {
    pub fn new(entries: BTreeMap<ObjectId, String>) -> Result<Self> {
        validate_entries(&entries, MAX_SERIALIZED_BYTES)?;
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &BTreeMap<ObjectId, String> {
        &self.entries
    }

    pub fn encode(&self) -> Vec<u8> {
        let pairs = self
            .entries
            .iter()
            .map(|(id, agent)| {
                let mut key = Vec::new();
                encode_text(&mut key, &id.hex());
                let mut value = Vec::new();
                encode_text(&mut value, agent);
                (key, value)
            })
            .collect();
        let mut out = Vec::new();
        encode_map_sorted(&mut out, pairs);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Self::decode_with_limit(bytes, MAX_SERIALIZED_BYTES)
    }

    fn decode_with_limit(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        if bytes.len() > max_bytes {
            return Err(Error::Corrupt(
                "provenance serialized size limit".into(),
            ));
        }
        let mut reader = Reader::new(bytes);
        let count = reader.map()?;
        if count > MAX_ENTRIES {
            return Err(Error::Corrupt("provenance entry limit".into()));
        }

        let mut entries = BTreeMap::new();
        let mut last = None;
        for _ in 0..count {
            let encoded_id =
                reader.text_map_key_bounded(&mut last, 64, "provenance object id")?;
            if encoded_id.len() != 64 {
                return Err(Error::Corrupt("provenance object id length".into()));
            }
            let id = ObjectId::from_hex(&encoded_id)
                .map_err(|error| Error::Corrupt(format!("provenance object id: {error}")))?;
            if id.hex() != encoded_id {
                return Err(Error::Corrupt(
                    "provenance object ids must be lowercase hex".into(),
                ));
            }
            let agent = reader.text_bounded(
                MAX_LABEL_BYTES,
                "provenance attribution label",
            )?;
            if entries.insert(id, agent).is_some() {
                return Err(Error::Corrupt("duplicate provenance object id".into()));
            }
        }
        if !reader.at_end() {
            return Err(Error::Corrupt("provenance trailing bytes".into()));
        }

        let manifest = Self { entries };
        validate_entries(&manifest.entries, max_bytes)
            .map_err(|error| Error::Corrupt(error.to_string()))?;
        if manifest.encode() != bytes {
            return Err(Error::Corrupt("non-canonical provenance encoding".into()));
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn attribution_and_canonical_payload_limits_apply_before_publication() {
        let oversized_label = BTreeMap::from([(
            ObjectId([1; 32]),
            "x".repeat(MAX_LABEL_BYTES + 1),
        )]);
        assert!(ProvenanceManifest::new(oversized_label).is_err());

        let manifest = ProvenanceManifest::new(BTreeMap::from([(
            ObjectId([2; 32]),
            "agent".into(),
        )]))
        .unwrap();
        let canonical = manifest.encode();
        let error = ProvenanceManifest::decode_with_limit(&canonical, canonical.len() - 1)
            .unwrap_err();
        assert!(error.to_string().contains("serialized size limit"));
    }

    #[test]
    fn decoder_rejects_an_oversized_label_before_copying_it() {
        let mut key = Vec::new();
        encode_text(&mut key, &ObjectId([3; 32]).hex());
        let mut label = Vec::new();
        encode_text(&mut label, &"x".repeat(MAX_LABEL_BYTES + 1));
        let mut encoded = Vec::new();
        encode_map_sorted(&mut encoded, vec![(key, label)]);

        let error = ProvenanceManifest::decode(&encoded).unwrap_err();
        assert!(error.to_string().contains("attribution label"));
    }
}
