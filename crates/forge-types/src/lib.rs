//! Shared identifiers, errors, and result types for ForgeFS.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// BLAKE3-256 of a complete on-disk object file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub const ZERO: ObjectId = ObjectId([0u8; 32]);

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ObjectId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn hex(&self) -> String {
        hex_encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let raw = hex_decode(s.trim())?;
        if raw.len() != 32 {
            return Err(Error::Invalid(format!(
                "object id hex must be 64 chars, got {}",
                s.len()
            )));
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&raw);
        Ok(ObjectId(id))
    }

    pub fn shard_dirs(&self) -> (String, String) {
        let h = self.hex();
        (h[0..2].to_string(), h[2..4].to_string())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.hex())
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(Error::Invalid("odd hex length".into()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::Invalid("invalid hex digit".into())),
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectType {
    Blob = 0x01,
    Tree = 0x02,
    Commit = 0x03,
    Conflict = 0x04,
    Snapshot = 0x05,
}

impl ObjectType {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0x01 => Ok(Self::Blob),
            0x02 => Ok(Self::Tree),
            0x03 => Ok(Self::Commit),
            0x04 => Ok(Self::Conflict),
            0x05 => Ok(Self::Snapshot),
            other => Err(Error::Corrupt(format!("unknown object type 0x{other:02x}"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Conflict => "conflict",
            Self::Snapshot => "snapshot",
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Blob = 1,
    Tree = 2,
}

impl EntryKind {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::Blob),
            2 => Ok(Self::Tree),
            other => Err(Error::Invalid(format!("unknown tree entry kind {other}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CasResult {
    Updated {
        name: String,
        oid: ObjectId,
    },
    Forked {
        requested: String,
        fork: String,
        ours: ObjectId,
        theirs: ObjectId,
    },
    Noop {
        name: String,
        oid: ObjectId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRow {
    pub name: String,
    pub oid: ObjectId,
    pub kind: String,
    pub protected: bool,
    pub sealed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("denied: {0}")]
    Denied(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("sealed: {0}")]
    Sealed(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("busy: {0}")]
    Busy(String),
    #[error("conflict object {0}")]
    MergeConflict(ObjectId),
    #[error("io: {0}")]
    Io(String),
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("cap: {0}")]
    Cap(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::Denied(_) => "denied",
            Error::NotFound(_) => "not_found",
            Error::Sealed(_) => "sealed",
            Error::Invalid(_) => "invalid",
            Error::Corrupt(_) => "corrupt",
            Error::Busy(_) => "busy",
            Error::MergeConflict(_) => "conflict",
            Error::Io(_) => "internal",
            Error::Sqlite(_) => "internal",
            Error::Cap(_) => "denied",
            Error::Internal(_) => "internal",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Directory that holds a forge (`.forge/`).
#[derive(Clone, Debug)]
pub struct ForgeDir(pub PathBuf);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let id = ObjectId([0xab; 32]);
        let h = id.hex();
        assert_eq!(h.len(), 64);
        assert_eq!(ObjectId::from_hex(&h).unwrap(), id);
    }

    #[test]
    fn shard_dirs() {
        let mut raw = [0u8; 32];
        raw[0] = 0xde;
        raw[1] = 0xad;
        let id = ObjectId(raw);
        let (a, b) = id.shard_dirs();
        assert_eq!(a, "de");
        assert_eq!(b, "ad");
    }
}
