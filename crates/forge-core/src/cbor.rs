//! Minimal canonical CBOR (RFC 8949 definite, shortest, sorted map keys).
//! Only the subset Forge objects need.

use forge_types::{Error, Result};

pub fn u8_len(n: u64) -> u8 {
    if n < 24 {
        n as u8
    } else if n <= 0xff {
        24
    } else if n <= 0xffff {
        25
    } else if n <= 0xffff_ffff {
        26
    } else {
        27
    }
}

fn push_uint(out: &mut Vec<u8>, major: u8, n: u64) {
    let ai = u8_len(n);
    out.push((major << 5) | ai);
    match ai {
        24 => out.push(n as u8),
        25 => out.extend_from_slice(&(n as u16).to_be_bytes()),
        26 => out.extend_from_slice(&(n as u32).to_be_bytes()),
        27 => out.extend_from_slice(&n.to_be_bytes()),
        _ => {}
    }
}

pub fn encode_u64(out: &mut Vec<u8>, n: u64) {
    push_uint(out, 0, n);
}

pub fn encode_bool(out: &mut Vec<u8>, v: bool) {
    out.push(if v { 0xf5 } else { 0xf4 });
}

pub fn encode_null(out: &mut Vec<u8>) {
    out.push(0xf6);
}

pub fn encode_bytes(out: &mut Vec<u8>, b: &[u8]) {
    push_uint(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn encode_text(out: &mut Vec<u8>, s: &str) {
    push_uint(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn encode_array_header(out: &mut Vec<u8>, n: u64) {
    push_uint(out, 4, n);
}

pub fn encode_map_header(out: &mut Vec<u8>, n: u64) {
    push_uint(out, 5, n);
}

/// Encode a map from already-encoded key/value pairs, sorting by key bytes.
pub fn encode_map_sorted(out: &mut Vec<u8>, mut pairs: Vec<(Vec<u8>, Vec<u8>)>) {
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    encode_map_header(out, pairs.len() as u64);
    for (k, v) in pairs {
        out.extend_from_slice(&k);
        out.extend_from_slice(&v);
    }
}

pub fn text_key(s: &str) -> Vec<u8> {
    let mut k = Vec::new();
    encode_text(&mut k, s);
    k
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn at_end(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::Corrupt("cbor length overflow".into()))?;
        if end > self.buf.len() {
            return Err(Error::Corrupt("cbor truncated".into()));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn take_uint(&mut self, ai: u8) -> Result<u64> {
        // RFC 8949 §4.2.1: shortest form only. Longer encodings of the same
        // integer would otherwise yield a different ObjectId for one value.
        match ai {
            0..=23 => Ok(ai as u64),
            24 => {
                let n = self.take(1)?[0] as u64;
                if n < 24 {
                    return Err(Error::Corrupt("non-canonical uint".into()));
                }
                Ok(n)
            }
            25 => {
                let b = self.take(2)?;
                let n = u16::from_be_bytes([b[0], b[1]]) as u64;
                if n <= 0xff {
                    return Err(Error::Corrupt("non-canonical uint".into()));
                }
                Ok(n)
            }
            26 => {
                let b = self.take(4)?;
                let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
                if n <= 0xffff {
                    return Err(Error::Corrupt("non-canonical uint".into()));
                }
                Ok(n)
            }
            27 => {
                let b = self.take(8)?;
                let n = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                if n <= 0xffff_ffff {
                    return Err(Error::Corrupt("non-canonical uint".into()));
                }
                Ok(n)
            }
            31 => Err(Error::Corrupt("indefinite cbor is not canonical".into())),
            _ => Err(Error::Corrupt("cbor additional info".into())),
        }
    }

    fn header(&mut self) -> Result<(u8, u8)> {
        let b = self.take(1)?[0];
        let major = b >> 5;
        let ai = b & 0x1f;
        if major == 6 {
            return Err(Error::Corrupt("cbor tags are not canonical here".into()));
        }
        Ok((major, ai))
    }

    fn bounded_len(&self, n: u64, what: &str) -> Result<usize> {
        let n = usize::try_from(n).map_err(|_| Error::Corrupt(format!("{what} length overflow")))?;
        if n > self.remaining().len() {
            return Err(Error::Corrupt(format!("{what} length exceeds input")));
        }
        Ok(n)
    }

    /// Map keys must be strictly increasing by **encoded** CBOR bytes (RFC 8949).
    pub fn text_map_key(&mut self, last: &mut Option<Vec<u8>>) -> Result<String> {
        let start = self.pos;
        let k = self.text()?;
        let encoded = self.buf[start..self.pos].to_vec();
        if let Some(prev) = last.as_ref() {
            if encoded.as_slice() <= prev.as_slice() {
                return Err(Error::Corrupt("cbor map keys not strictly sorted".into()));
            }
        }
        *last = Some(encoded);
        Ok(k)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let (major, ai) = self.header()?;
        if major != 0 {
            return Err(Error::Corrupt("expected uint".into()));
        }
        self.take_uint(ai)
    }

    pub fn bool(&mut self) -> Result<bool> {
        let b = self.take(1)?[0];
        match b {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => Err(Error::Corrupt("expected bool".into())),
        }
    }

    pub fn null_or<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.pos < self.buf.len() && self.buf[self.pos] == 0xf6 {
            self.pos += 1;
            return Ok(None);
        }
        Ok(Some(read(self)?))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let (major, ai) = self.header()?;
        if major != 2 {
            return Err(Error::Corrupt("expected bytes".into()));
        }
        let raw = self.take_uint(ai)?;
        let n = self.bounded_len(raw, "bytes")?;
        self.take(n)
    }

    pub fn bstr32(&mut self) -> Result<[u8; 32]> {
        let b = self.bytes()?;
        if b.len() != 32 {
            return Err(Error::Corrupt(format!(
                "expected 32-byte bstr, got {}",
                b.len()
            )));
        }
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        Ok(a)
    }

    pub fn text(&mut self) -> Result<String> {
        let (major, ai) = self.header()?;
        if major != 3 {
            return Err(Error::Corrupt("expected text".into()));
        }
        let raw = self.take_uint(ai)?;
        let n = self.bounded_len(raw, "text")?;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| Error::Corrupt("utf8".into()))
    }

    pub fn array(&mut self) -> Result<u64> {
        let (major, ai) = self.header()?;
        if major != 4 {
            return Err(Error::Corrupt("expected array".into()));
        }
        let n = self.take_uint(ai)?;
        if n > self.remaining().len() as u64 {
            return Err(Error::Corrupt("array count exceeds input".into()));
        }
        Ok(n)
    }

    pub fn map(&mut self) -> Result<u64> {
        let (major, ai) = self.header()?;
        if major != 5 {
            return Err(Error::Corrupt("expected map".into()));
        }
        let n = self.take_uint(ai)?;
        if n > (self.remaining().len() / 2) as u64 {
            return Err(Error::Corrupt("map count exceeds input".into()));
        }
        Ok(n)
    }
}
