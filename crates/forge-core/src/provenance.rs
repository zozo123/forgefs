//! Canonical payload stored in a Snapshot's provenance Blob.

use crate::cbor::{encode_map_sorted, encode_text, Reader};
use forge_types::{Error, ObjectId, Result};
use std::collections::BTreeMap;

const MAX_ENTRIES: u64 = 1_000_000;

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
        if entries.len() > MAX_ENTRIES as usize {
            return Err(Error::Invalid("provenance entry limit".into()));
        }
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
        let mut reader = Reader::new(bytes);
        let count = reader.map()?;
        if count > MAX_ENTRIES {
            return Err(Error::Corrupt("provenance entry limit".into()));
        }

        let mut entries = BTreeMap::new();
        let mut last = None;
        for _ in 0..count {
            let encoded_id = reader.text_map_key(&mut last)?;
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
            let agent = reader.text()?;
            if entries.insert(id, agent).is_some() {
                return Err(Error::Corrupt("duplicate provenance object id".into()));
            }
        }
        if !reader.at_end() {
            return Err(Error::Corrupt("provenance trailing bytes".into()));
        }

        let manifest = Self { entries };
        if manifest.encode() != bytes {
            return Err(Error::Corrupt("non-canonical provenance encoding".into()));
        }
        Ok(manifest)
    }
}
