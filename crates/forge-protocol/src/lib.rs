use forge_types::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub v: u8,
    pub id: u64,
    pub op: String,
    pub cap: String,
    pub body: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub v: u8,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<ErrBody>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrBody {
    pub code: String,
    pub msg: String,
}

impl Response {
    pub fn ok(id: u64, body: Value) -> Self {
        Self {
            v: 1,
            id,
            ok: true,
            body: Some(body),
            err: None,
        }
    }

    pub fn err(id: u64, e: &Error) -> Self {
        Self {
            v: 1,
            id,
            ok: false,
            body: None,
            err: Some(ErrBody {
                code: e.code().into(),
                msg: e.to_string(),
            }),
        }
    }
}

pub fn write_frame(w: &mut impl Write, v: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(v).map_err(|e| Error::Invalid(e.to_string()))?;
    let n = bytes.len() as u32;
    w.write_all(&n.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Hard cap on a single frame body. A length prefix larger than this is
/// rejected before any allocation, so a 4-byte header cannot make the server
/// reserve gigabytes.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

fn decode_frame_len(prefix: [u8; 4]) -> Result<usize> {
    let n = u32::from_be_bytes(prefix) as usize;
    if n > MAX_FRAME_BYTES {
        return Err(Error::Invalid("frame too large".into()));
    }
    Ok(n)
}

/// Read the 4-byte big-endian length prefix of a frame.
///
/// Split out from [`read_frame`] so a server can adjust its socket read
/// deadline between the header and the body: the header commits the peer to
/// sending a frame, and everything after that point deserves a stricter
/// deadline than an idle connection does.
pub fn read_frame_len(r: &mut impl Read) -> Result<usize> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix)?;
    decode_frame_len(prefix)
}

/// Read the remaining 3 bytes of a length prefix whose first byte the caller
/// already consumed.
///
/// A server that wants to distinguish \"connection is idle\" from \"peer started
/// a frame and stalled\" has to observe the very first byte on its own, then
/// tighten the deadline before reading byte two. That is why this exists.
pub fn read_frame_len_after(r: &mut impl Read, first: u8) -> Result<usize> {
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest)?;
    decode_frame_len([first, rest[0], rest[1], rest[2]])
}

/// Read exactly `n` bytes of frame body, `n` having come from one of the
/// length-prefix readers above (and therefore already checked against
/// [`MAX_FRAME_BYTES`]).
pub fn read_frame_body(r: &mut impl Read, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read one whole frame. Unchanged behaviour for callers that have no reason to
/// retune anything mid-frame.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>> {
    let n = read_frame_len(r)?;
    read_frame_body(r, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_read_matches_whole_frame_read() {
        let mut framed = Vec::new();
        write_frame(&mut framed, &serde_json::json!({"a": 1})).unwrap();

        let mut whole = &framed[..];
        let body = read_frame(&mut whole).unwrap();

        let mut split = &framed[..];
        let mut first = [0u8; 1];
        split.read_exact(&mut first).unwrap();
        let n = read_frame_len_after(&mut split, first[0]).unwrap();
        assert_eq!(read_frame_body(&mut split, n).unwrap(), body);
    }

    #[test]
    fn oversized_length_prefix_is_rejected_before_allocating() {
        let n = (MAX_FRAME_BYTES + 1) as u32;
        let mut bytes = &n.to_be_bytes()[..];
        assert!(read_frame_len(&mut bytes).is_err());

        let split = n.to_be_bytes();
        let mut rest = &split[1..];
        assert!(read_frame_len_after(&mut rest, split[0]).is_err());
    }
}
