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

pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let n = u32::from_be_bytes(lenb) as usize;
    if n > 32 * 1024 * 1024 {
        return Err(Error::Invalid("frame too large".into()));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}
