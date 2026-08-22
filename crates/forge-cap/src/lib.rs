//! Tahoe-style HMAC macaroons. Authority can only be attenuated.

use forge_types::{hex_decode, hex_encode, Error, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashSet;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Write,
    Branch,
    Merge,
    Grant,
    Seal,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Branch => "branch",
            Op::Merge => "merge",
            Op::Grant => "grant",
            Op::Seal => "seal",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "read" => Ok(Op::Read),
            "write" => Ok(Op::Write),
            "branch" => Ok(Op::Branch),
            "merge" => Ok(Op::Merge),
            "grant" => Ok(Op::Grant),
            "seal" => Ok(Op::Seal),
            other => Err(Error::Cap(format!("unknown op {other}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cap {
    pub loc: String,
    pub id: String,
    pub caveats: Vec<String>,
    pub sig: [u8; 32],
    pub ops: HashSet<Op>,
    pub ref_globs: Vec<String>,
    pub ref_not: Vec<String>,
    pub time_le: Option<u64>,
    pub agent: Option<String>,
}

impl Cap {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = prefix_bytes(&self.loc, &self.id);
        for c in &self.caveats {
            let b = c.as_bytes();
            v.extend_from_slice(&(b.len() as u16).to_be_bytes());
            v.extend_from_slice(b);
        }
        v.extend_from_slice(&self.sig);
        v
    }

    pub fn to_token(&self) -> String {
        format!("fmac1_{}", hex_encode(&self.encode()))
    }

    pub fn from_token(s: &str) -> Result<Self> {
        let raw = if let Some(h) = s.strip_prefix("fmac1_") {
            hex_decode(h)?
        } else {
            hex_decode(s)?
        };
        decode_cap(&raw)
    }

    pub fn allows(&self, op: Op, ref_name: Option<&str>, now_ms: u64) -> Result<()> {
        if !self.ops.contains(&op) {
            return Err(Error::Denied(format!("cap missing op {}", op.as_str())));
        }
        if let Some(t) = self.time_le {
            if now_ms > t {
                return Err(Error::Denied("cap expired".into()));
            }
        }
        if let Some(name) = ref_name {
            if !self.ref_not.is_empty() {
                for n in &self.ref_not {
                    if ref_matches(n, name) {
                        return Err(Error::Denied(format!("cap excludes ref {name}")));
                    }
                }
            }
            if !self.ref_globs.is_empty() {
                if !self.ref_globs.iter().any(|g| ref_matches(g, name)) {
                    return Err(Error::Denied(format!("cap does not cover ref {name}")));
                }
            }
        }
        Ok(())
    }

    pub fn agent_id(&self) -> &str {
        self.agent.as_deref().unwrap_or("anon")
    }
}

pub fn ref_matches(glob: &str, name: &str) -> bool {
    if let Some(prefix) = glob.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        glob == name
    }
}

fn prefix_bytes(loc: &str, id: &str) -> Vec<u8> {
    let mut v = b"FMAC".to_vec();
    v.push(1);
    v.extend_from_slice(&(loc.len() as u16).to_be_bytes());
    v.extend_from_slice(loc.as_bytes());
    v.extend_from_slice(&(id.len() as u16).to_be_bytes());
    v.extend_from_slice(id.as_bytes());
    v
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut sig = [0u8; 32];
    sig.copy_from_slice(&out);
    sig
}

fn sign_chain(root: &[u8], loc: &str, id: &str, caveats: &[String]) -> [u8; 32] {
    let mut sig = hmac(root, &prefix_bytes(loc, id));
    for c in caveats {
        let mut buf = Vec::new();
        let b = c.as_bytes();
        buf.extend_from_slice(&(b.len() as u16).to_be_bytes());
        buf.extend_from_slice(b);
        sig = hmac(&sig, &buf);
    }
    sig
}

fn parse_caveats(
    caveats: &[String],
) -> Result<(HashSet<Op>, Vec<String>, Vec<String>, Option<u64>, Option<String>)> {
    let mut ops: Option<HashSet<Op>> = None;
    let mut globs = Vec::new();
    let mut nots = Vec::new();
    let mut time_le = None;
    let mut agent = None;
    for c in caveats {
        if let Some(rest) = c.strip_prefix("ops=") {
            let mut set = HashSet::new();
            for part in rest.split(',') {
                let p = part.trim();
                if p.is_empty() {
                    continue;
                }
                set.insert(Op::parse(p)?);
            }
            ops = Some(match ops {
                None => set,
                Some(prev) => prev.intersection(&set).cloned().collect(),
            });
        } else if let Some(rest) = c.strip_prefix("ref!=") {
            nots.push(rest.to_string());
        } else if let Some(rest) = c.strip_prefix("ref=") {
            globs.push(rest.to_string());
        } else if let Some(rest) = c.strip_prefix("time<=") {
            time_le = Some(
                rest.parse::<u64>()
                    .map_err(|_| Error::Cap("bad time<=".into()))?,
            );
        } else if let Some(rest) = c.strip_prefix("agent=") {
            agent = Some(rest.to_string());
        } else {
            return Err(Error::Cap(format!("unknown caveat {c}")));
        }
    }
    Ok((ops.unwrap_or_default(), globs, nots, time_le, agent))
}

fn decode_cap(raw: &[u8]) -> Result<Cap> {
    if raw.len() < 4 + 1 + 2 + 2 + 32 {
        return Err(Error::Cap("cap truncated".into()));
    }
    if &raw[0..4] != b"FMAC" {
        return Err(Error::Cap("bad magic".into()));
    }
    if raw[4] != 1 {
        return Err(Error::Cap("bad version".into()));
    }
    let mut i = 5;
    let loc_len = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
    i += 2;
    if i + loc_len + 2 > raw.len() {
        return Err(Error::Cap("cap loc".into()));
    }
    let loc = String::from_utf8(raw[i..i + loc_len].to_vec()).map_err(|_| Error::Cap("utf8".into()))?;
    i += loc_len;
    let id_len = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
    i += 2;
    if i + id_len + 32 > raw.len() {
        return Err(Error::Cap("cap id".into()));
    }
    let id = String::from_utf8(raw[i..i + id_len].to_vec()).map_err(|_| Error::Cap("utf8".into()))?;
    i += id_len;
    let mut caveats = Vec::new();
    while i + 32 < raw.len() {
        if i + 2 > raw.len() - 32 {
            break;
        }
        let n = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
        i += 2;
        if i + n + 32 > raw.len() {
            return Err(Error::Cap("cap caveat".into()));
        }
        let s = String::from_utf8(raw[i..i + n].to_vec()).map_err(|_| Error::Cap("utf8".into()))?;
        caveats.push(s);
        i += n;
    }
    if i + 32 != raw.len() {
        return Err(Error::Cap("cap trailing".into()));
    }
    let mut sig = [0u8; 32];
    sig.copy_from_slice(&raw[i..]);
    let (ops, ref_globs, ref_not, time_le, agent) = parse_caveats(&caveats)?;
    Ok(Cap {
        loc,
        id,
        caveats,
        sig,
        ops,
        ref_globs,
        ref_not,
        time_le,
        agent,
    })
}

pub fn verify(root: &[u8], cap: &Cap) -> Result<()> {
    let expect = sign_chain(root, &cap.loc, &cap.id, &cap.caveats);
    if expect != cap.sig {
        return Err(Error::Cap("bad signature".into()));
    }
    Ok(())
}

pub fn mint(root: &[u8], loc: &str, id: &str, caveats: Vec<String>) -> Result<Cap> {
    let sig = sign_chain(root, loc, id, &caveats);
    let (ops, ref_globs, ref_not, time_le, agent) = parse_caveats(&caveats)?;
    Ok(Cap {
        loc: loc.to_string(),
        id: id.to_string(),
        caveats,
        sig,
        ops,
        ref_globs,
        ref_not,
        time_le,
        agent,
    })
}

/// Attenuate by appending caveats to *this* cap. Never re-signs from root.
pub fn attenuate(root: &[u8], cap: &Cap, extra: Vec<String>) -> Result<Cap> {
    verify(root, cap)?;
    let mut caveats = cap.caveats.clone();
    caveats.extend(extra);
    mint(root, &cap.loc, &cap.id, caveats)
}

pub fn mint_root(root: &[u8]) -> Result<Cap> {
    mint(
        root,
        "forge",
        "root",
        vec!["ops=read,write,branch,merge,grant,seal".into()],
    )
}

pub fn mint_integrator(root: &[u8]) -> Result<Cap> {
    mint(
        root,
        "forge",
        "root",
        vec![
            "ops=read,write,branch,merge,grant,seal".into(),
            "ops=read,merge,seal,grant".into(),
            "ref=main".into(),
            "ref=tags/*".into(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_roundtrip_and_ops() {
        let key = [7u8; 32];
        let cap = mint_root(&key).unwrap();
        verify(&key, &cap).unwrap();
        cap.allows(Op::Write, Some("heads/x"), 0).unwrap();
        cap.allows(Op::Seal, Some("main"), 0).unwrap();
        let tok = cap.to_token();
        let cap2 = Cap::from_token(&tok).unwrap();
        verify(&key, &cap2).unwrap();
        assert_eq!(cap, cap2);
    }

    #[test]
    fn attenuate_cannot_escalate() {
        let key = [7u8; 32];
        let root = mint_root(&key).unwrap();
        let agent = attenuate(
            &key,
            &root,
            vec![
                "ops=read,write,branch".into(),
                "ref=heads/agents/alice/*".into(),
                "ref!=main".into(),
                "agent=alice".into(),
            ],
        )
        .unwrap();
        agent
            .allows(Op::Write, Some("heads/agents/alice/01"), 0)
            .unwrap();
        assert!(agent.allows(Op::Seal, Some("main"), 0).is_err());
        assert!(agent.allows(Op::Write, Some("main"), 0).is_err());
        assert!(agent.allows(Op::Write, Some("heads/agents/bob/1"), 0).is_err());
    }

    #[test]
    fn integrator_covers_main_and_tags() {
        let key = [1u8; 32];
        let c = mint_integrator(&key).unwrap();
        c.allows(Op::Seal, Some("main"), 0).unwrap();
        c.allows(Op::Seal, Some("tags/v1.0"), 0).unwrap();
        assert!(c.allows(Op::Seal, Some("heads/x"), 0).is_err());
        assert!(c.allows(Op::Write, Some("main"), 0).is_err());
    }

    #[test]
    fn bad_key_fails() {
        let cap = mint_root(&[1u8; 32]).unwrap();
        assert!(verify(&[2u8; 32], &cap).is_err());
    }
}
