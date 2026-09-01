//! Macaroon-style HMAC capabilities with monotonic attenuation.
//! A holder can append caveats using the current signature; the root secret is
//! needed only to mint and verify credentials.

use forge_types::{hex_decode, hex_encode, Error, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};

type HmacSha256 = Hmac<Sha256>;

const MAX_TOKEN_BYTES: usize = 64 * 1024;
const MAX_CAVEATS: usize = 256;
const MAX_FIELD_BYTES: usize = u16::MAX as usize;

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
    // Authorization state is derived exclusively from signed caveats and must
    // never be caller-mutable independently of those authenticated bytes.
    ops: HashSet<Op>,
    /// Positive global reference caveats. Each inner set is OR; all sets are ANDed.
    ref_sets: Vec<Vec<String>>,
    /// Operation-specific positive reference caveats. These are additionally
    /// ANDed with the global reference caveats for that operation.
    op_ref_sets: HashMap<Op, Vec<Vec<String>>>,
    ref_not: Vec<String>,
    time_le: Option<u64>,
    agent: Option<String>,
    agent_conflict: bool,
}

impl Cap {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = prefix_bytes(&self.loc, &self.id);
        for c in &self.caveats {
            append_caveat_bytes(&mut v, c);
        }
        v.extend_from_slice(&self.sig);
        v
    }

    pub fn to_token(&self) -> String {
        format!("fmac1_{}", hex_encode(&self.encode()))
    }

    pub fn from_token(s: &str) -> Result<Self> {
        let raw_hex = s.strip_prefix("fmac1_").unwrap_or(s);
        if raw_hex.len() > MAX_TOKEN_BYTES * 2 {
            return Err(Error::Cap("cap too large".into()));
        }
        let raw = hex_decode(raw_hex)?;
        decode_cap(&raw)
    }

    pub fn allows(&self, op: Op, ref_name: Option<&str>, now_ms: u64) -> Result<()> {
        if self.agent_conflict {
            return Err(Error::Denied("cap has conflicting agent caveats".into()));
        }
        if !self.ops.contains(&op) {
            return Err(Error::Denied(format!("cap missing op {}", op.as_str())));
        }
        if let Some(t) = self.time_le {
            if now_ms > t {
                return Err(Error::Denied("cap expired".into()));
            }
        }
        if let Some(name) = ref_name {
            if self.ref_not.iter().any(|n| ref_matches(n, name)) {
                return Err(Error::Denied(format!("cap excludes ref {name}")));
            }
            for allowed_set in &self.ref_sets {
                if !matches_any(allowed_set, name) {
                    return Err(Error::Denied(format!("cap does not cover ref {name}")));
                }
            }
            if let Some(scoped_sets) = self.op_ref_sets.get(&op) {
                for allowed_set in scoped_sets {
                    if !matches_any(allowed_set, name) {
                        return Err(Error::Denied(format!(
                            "cap does not allow {} on ref {name}",
                            op.as_str()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn agent_id(&self) -> &str {
        self.agent.as_deref().unwrap_or("anon")
    }

    /// Whether a raw object ID can be authorized without a reference name.
    /// Any positive, operation-specific, or negative ref caveat makes the
    /// scope name-dependent, so an OID cannot safely be checked against it.
    pub fn has_unrestricted_ref_scope(&self) -> bool {
        self.ref_sets.is_empty() && self.op_ref_sets.is_empty() && self.ref_not.is_empty()
    }
}

fn matches_any(globs: &[String], name: &str) -> bool {
    globs.iter().any(|g| ref_matches(g, name))
}

pub fn ref_matches(glob: &str, name: &str) -> bool {
    if let Some(prefix) = glob.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        glob == name
    }
}

fn checked_u16_len(s: &str, what: &str) -> Result<u16> {
    if s.len() > MAX_FIELD_BYTES {
        return Err(Error::Cap(format!("{what} too large")));
    }
    Ok(s.len() as u16)
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

fn append_caveat_bytes(v: &mut Vec<u8>, c: &str) {
    let b = c.as_bytes();
    v.extend_from_slice(&(b.len() as u16).to_be_bytes());
    v.extend_from_slice(b);
}

fn caveat_mac_input(c: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + c.len());
    append_caveat_bytes(&mut v, c);
    v
}

fn mac(key: &[u8], data: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(data);
    mac
}

fn finalize_mac(mac: HmacSha256) -> [u8; 32] {
    let out = mac.finalize().into_bytes();
    let mut sig = [0u8; 32];
    sig.copy_from_slice(&out);
    sig
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    finalize_mac(mac(key, data))
}

fn final_chain_mac(root: &[u8], loc: &str, id: &str, caveats: &[String]) -> HmacSha256 {
    let Some((last, prior)) = caveats.split_last() else {
        return mac(root, &prefix_bytes(loc, id));
    };

    let mut sig = hmac(root, &prefix_bytes(loc, id));
    for c in prior {
        sig = hmac(&sig, &caveat_mac_input(c));
    }
    mac(&sig, &caveat_mac_input(last))
}

fn sign_chain(root: &[u8], loc: &str, id: &str, caveats: &[String]) -> [u8; 32] {
    finalize_mac(final_chain_mac(root, loc, id, caveats))
}

struct ParsedCaveats {
    ops: HashSet<Op>,
    ref_sets: Vec<Vec<String>>,
    op_ref_sets: HashMap<Op, Vec<Vec<String>>>,
    ref_not: Vec<String>,
    time_le: Option<u64>,
    agent: Option<String>,
    agent_conflict: bool,
}

fn parse_ref_set(rest: &str, kind: &str) -> Result<Vec<String>> {
    let set: Vec<String> = rest
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if set.is_empty() {
        return Err(Error::Cap(format!("empty {kind} caveat")));
    }
    Ok(set)
}

fn parse_caveats(caveats: &[String]) -> Result<ParsedCaveats> {
    if caveats.len() > MAX_CAVEATS {
        return Err(Error::Cap("too many caveats".into()));
    }
    let mut ops: Option<HashSet<Op>> = None;
    let mut ref_sets = Vec::new();
    let mut op_ref_sets: HashMap<Op, Vec<Vec<String>>> = HashMap::new();
    let mut nots = Vec::new();
    let mut time_le: Option<u64> = None;
    let mut agent: Option<String> = None;
    let mut agent_conflict = false;

    for c in caveats {
        checked_u16_len(c, "caveat")?;
        if let Some(rest) = c.strip_prefix("ops=") {
            let mut set = HashSet::new();
            for part in rest.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    set.insert(Op::parse(p)?);
                }
            }
            ops = Some(match ops {
                None => set,
                Some(prev) => prev.intersection(&set).copied().collect(),
            });
        } else if let Some(rest) = c.strip_prefix("allow=") {
            let (op_s, refs) = rest
                .split_once(':')
                .ok_or_else(|| Error::Cap("allow caveat needs allow=<op>:<refs>".into()))?;
            let op = Op::parse(op_s.trim())?;
            let set = parse_ref_set(refs, "allow")?;
            op_ref_sets.entry(op).or_default().push(set);
        } else if let Some(rest) = c.strip_prefix("ref!=") {
            nots.extend(parse_ref_set(rest, "ref!=")?);
        } else if let Some(rest) = c.strip_prefix("ref=") {
            ref_sets.push(parse_ref_set(rest, "ref=")?);
        } else if let Some(rest) = c.strip_prefix("time<=") {
            let t = rest
                .parse::<u64>()
                .map_err(|_| Error::Cap("bad time<=".into()))?;
            time_le = Some(time_le.map_or(t, |prev| prev.min(t)));
        } else if let Some(rest) = c.strip_prefix("agent=") {
            if rest.is_empty() {
                return Err(Error::Cap("empty agent caveat".into()));
            }
            match &agent {
                None => agent = Some(rest.to_string()),
                Some(prev) if prev == rest => {}
                Some(_) => agent_conflict = true,
            }
        } else {
            return Err(Error::Cap(format!("unknown caveat {c}")));
        }
    }

    Ok(ParsedCaveats {
        ops: ops.unwrap_or_default(),
        ref_sets,
        op_ref_sets,
        ref_not: nots,
        time_le,
        agent,
        agent_conflict,
    })
}

fn cap_from_parts(loc: String, id: String, caveats: Vec<String>, sig: [u8; 32]) -> Result<Cap> {
    let parsed = parse_caveats(&caveats)?;
    Ok(Cap {
        loc,
        id,
        caveats,
        sig,
        ops: parsed.ops,
        ref_sets: parsed.ref_sets,
        op_ref_sets: parsed.op_ref_sets,
        ref_not: parsed.ref_not,
        time_le: parsed.time_le,
        agent: parsed.agent,
        agent_conflict: parsed.agent_conflict,
    })
}

fn decode_cap(raw: &[u8]) -> Result<Cap> {
    if raw.len() > MAX_TOKEN_BYTES {
        return Err(Error::Cap("cap too large".into()));
    }
    if raw.len() < 4 + 1 + 2 + 2 + 32 {
        return Err(Error::Cap("cap truncated".into()));
    }
    if &raw[0..4] != b"FMAC" || raw[4] != 1 {
        return Err(Error::Cap("bad cap header".into()));
    }
    let mut i = 5;
    let loc_len = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
    i += 2;
    if i + loc_len + 2 + 32 > raw.len() {
        return Err(Error::Cap("cap loc".into()));
    }
    let loc =
        String::from_utf8(raw[i..i + loc_len].to_vec()).map_err(|_| Error::Cap("utf8".into()))?;
    i += loc_len;
    let id_len = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
    i += 2;
    if i + id_len + 32 > raw.len() {
        return Err(Error::Cap("cap id".into()));
    }
    let id =
        String::from_utf8(raw[i..i + id_len].to_vec()).map_err(|_| Error::Cap("utf8".into()))?;
    i += id_len;

    let mut caveats = Vec::new();
    while i + 32 < raw.len() {
        if caveats.len() >= MAX_CAVEATS || i + 2 > raw.len() - 32 {
            return Err(Error::Cap("bad caveat framing".into()));
        }
        let n = u16::from_be_bytes([raw[i], raw[i + 1]]) as usize;
        i += 2;
        if i + n + 32 > raw.len() {
            return Err(Error::Cap("cap caveat".into()));
        }
        caveats.push(
            String::from_utf8(raw[i..i + n].to_vec()).map_err(|_| Error::Cap("utf8".into()))?,
        );
        i += n;
    }
    if i + 32 != raw.len() {
        return Err(Error::Cap("cap trailing".into()));
    }
    let mut sig = [0u8; 32];
    sig.copy_from_slice(&raw[i..]);
    cap_from_parts(loc, id, caveats, sig)
}

pub fn verify(root: &[u8], cap: &Cap) -> Result<()> {
    final_chain_mac(root, &cap.loc, &cap.id, &cap.caveats)
        .verify_slice(&cap.sig)
        .map_err(|_| Error::Cap("bad signature".into()))
}

pub fn mint(root: &[u8], loc: &str, id: &str, caveats: Vec<String>) -> Result<Cap> {
    checked_u16_len(loc, "location")?;
    checked_u16_len(id, "id")?;
    parse_caveats(&caveats)?;
    let sig = sign_chain(root, loc, id, &caveats);
    cap_from_parts(loc.to_string(), id.to_string(), caveats, sig)
}

/// Root-secret-free attenuation for callers that already hold a capability.
pub fn attenuate_holder(cap: &Cap, extra: Vec<String>) -> Result<Cap> {
    if cap.caveats.len() + extra.len() > MAX_CAVEATS {
        return Err(Error::Cap("too many caveats".into()));
    }
    let mut caveats = cap.caveats.clone();
    let mut sig = cap.sig;
    for c in extra {
        checked_u16_len(&c, "caveat")?;
        sig = hmac(&sig, &caveat_mac_input(&c));
        caveats.push(c);
    }
    cap_from_parts(cap.loc.clone(), cap.id.clone(), caveats, sig)
}

/// Compatibility wrapper. The root argument is deliberately unused: holders
/// attenuate from the current signature, never by re-signing from the root.
pub fn attenuate(_root: &[u8], cap: &Cap, extra: Vec<String>) -> Result<Cap> {
    attenuate_holder(cap, extra)
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
        "integrator",
        vec![
            "ops=read,merge,seal,grant".into(),
            "allow=read:main,heads/agents/*,forks/*,tags/*,conflicts/*".into(),
            "allow=merge:main".into(),
            "allow=seal:main,tags/*".into(),
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
        let cap2 = Cap::from_token(&cap.to_token()).unwrap();
        verify(&key, &cap2).unwrap();
        assert_eq!(cap, cap2);
    }

    #[test]
    fn holder_attenuates_without_root_key() {
        let key = [7u8; 32];
        let root = mint_root(&key).unwrap();
        let agent = attenuate_holder(
            &root,
            vec![
                "ops=read,write,branch".into(),
                "ref=heads/agents/alice/*".into(),
                "ref!=main".into(),
                "agent=alice".into(),
            ],
        )
        .unwrap();
        verify(&key, &agent).unwrap();
        agent
            .allows(Op::Write, Some("heads/agents/alice/01"), 0)
            .unwrap();
        assert!(agent.allows(Op::Seal, Some("main"), 0).is_err());
        assert!(agent.allows(Op::Write, Some("main"), 0).is_err());
        assert!(agent
            .allows(Op::Write, Some("heads/agents/bob/1"), 0)
            .is_err());
    }

    #[test]
    fn ref_caveats_intersect_not_union() {
        let key = [3u8; 32];
        let broad = mint(
            &key,
            "forge",
            "x",
            vec!["ops=read".into(), "ref=heads/*,main".into()],
        )
        .unwrap();
        broad.allows(Op::Read, Some("main"), 0).unwrap();
        broad.allows(Op::Read, Some("heads/a"), 0).unwrap();
        let narrow = attenuate_holder(&broad, vec!["ref=heads/alice/*".into()]).unwrap();
        narrow.allows(Op::Read, Some("heads/alice/1"), 0).unwrap();
        assert!(narrow.allows(Op::Read, Some("main"), 0).is_err());
        assert!(narrow.allows(Op::Read, Some("heads/bob/1"), 0).is_err());
    }

    #[test]
    fn operation_scopes_are_not_a_cartesian_product() {
        let key = [8u8; 32];
        let c = mint(
            &key,
            "forge",
            "scoped",
            vec![
                "ops=read,merge,seal".into(),
                "allow=read:heads/*,main,tags/*".into(),
                "allow=merge:main".into(),
                "allow=seal:main,tags/*".into(),
            ],
        )
        .unwrap();
        c.allows(Op::Read, Some("heads/alice/1"), 0).unwrap();
        c.allows(Op::Merge, Some("main"), 0).unwrap();
        c.allows(Op::Seal, Some("tags/v1"), 0).unwrap();
        assert!(c.allows(Op::Merge, Some("heads/alice/1"), 0).is_err());
        assert!(c.allows(Op::Seal, Some("heads/alice/1"), 0).is_err());
    }

    #[test]
    fn operation_scope_attenuation_only_shrinks() {
        let key = [9u8; 32];
        let broad = mint(
            &key,
            "forge",
            "scoped",
            vec!["ops=read".into(), "allow=read:heads/*,main".into()],
        )
        .unwrap();
        let narrow = attenuate_holder(&broad, vec!["allow=read:heads/alice/*".into()]).unwrap();
        narrow.allows(Op::Read, Some("heads/alice/1"), 0).unwrap();
        assert!(narrow.allows(Op::Read, Some("heads/bob/1"), 0).is_err());
        assert!(narrow.allows(Op::Read, Some("main"), 0).is_err());
    }

    #[test]
    fn raw_oid_scope_requires_no_ref_caveat_of_any_kind() {
        let key = [10u8; 32];
        let cases = [
            (vec!["ops=read", "time<=100"], true),
            (vec!["ops=read", "agent=reader"], true),
            (vec!["ops=read", "ref=main"], false),
            (vec!["ops=read", "allow=read:main"], false),
            (vec!["ops=read", "ref!=main"], false),
        ];

        let unscoped = mint(&key, "forge", "unscoped", vec!["ops=read".into()]).unwrap();
        assert!(unscoped.has_unrestricted_ref_scope());

        for (caveats, expected) in cases {
            let cap = mint(
                &key,
                "forge",
                "scope-test",
                caveats.into_iter().map(str::to_owned).collect(),
            )
            .unwrap();
            assert_eq!(
                cap.has_unrestricted_ref_scope(),
                expected,
                "caveats={:?}",
                cap.caveats
            );
        }
    }

    #[test]
    fn time_can_only_shrink() {
        let key = [4u8; 32];
        let c = mint(
            &key,
            "forge",
            "x",
            vec!["ops=read".into(), "time<=100".into()],
        )
        .unwrap();
        let later = attenuate_holder(&c, vec!["time<=1000".into()]).unwrap();
        assert_eq!(later.time_le, Some(100));
        assert!(later.allows(Op::Read, None, 101).is_err());
        let earlier = attenuate_holder(&c, vec!["time<=50".into()]).unwrap();
        assert_eq!(earlier.time_le, Some(50));
    }

    #[test]
    fn conflicting_agent_caveats_deny() {
        let key = [5u8; 32];
        let alice = mint(
            &key,
            "forge",
            "x",
            vec!["ops=read".into(), "agent=alice".into()],
        )
        .unwrap();
        let impossible = attenuate_holder(&alice, vec!["agent=bob".into()]).unwrap();
        verify(&key, &impossible).unwrap();
        assert!(impossible.allows(Op::Read, None, 0).is_err());
    }

    #[test]
    fn integrator_has_asymmetric_authority() {
        let key = [1u8; 32];
        let c = mint_integrator(&key).unwrap();
        c.allows(Op::Read, Some("heads/agents/alice/1"), 0).unwrap();
        c.allows(Op::Read, Some("forks/main/alice/1"), 0).unwrap();
        c.allows(Op::Merge, Some("main"), 0).unwrap();
        c.allows(Op::Seal, Some("main"), 0).unwrap();
        c.allows(Op::Seal, Some("tags/v1.0"), 0).unwrap();
        assert!(c
            .allows(Op::Merge, Some("heads/agents/alice/1"), 0)
            .is_err());
        assert!(c.allows(Op::Seal, Some("heads/agents/alice/1"), 0).is_err());
        assert!(c.allows(Op::Write, Some("main"), 0).is_err());
    }

    #[test]
    fn bad_key_fails() {
        let cap = mint_root(&[1u8; 32]).unwrap();
        assert!(verify(&[2u8; 32], &cap).is_err());
    }

    #[test]
    fn one_bit_forged_signature_fails() {
        let key = [11u8; 32];
        let mut cap = mint_root(&key).unwrap();
        verify(&key, &cap).unwrap();
        cap.sig[0] ^= 1;
        assert!(verify(&key, &cap).is_err());
    }

    /// Known-answer test for the capability wire format.
    ///
    /// Invariant: a fixed root secret and caveat chain must always produce the
    /// exact same token bytes. Every other test in this module signs and
    /// verifies within a single build, so a change to the HMAC construction --
    /// or to the digest implementation underneath it -- would round-trip
    /// cleanly while silently invalidating every capability already issued.
    /// Pinning the bytes turns that from an undetected break into a test
    /// failure, and makes any deliberate format change an explicit edit here.
    #[test]
    fn token_bytes_are_pinned_across_digest_implementations() {
        const ROOT_TOKEN: &str = "fmac1_464d4143010005666f7267650004726f6f7400266f70733d726561642c77726974652c6272616e63682c6d657267652c6772616e742c7365616c931478c6e832f60f352ae4e2ec8a18a521eedc8618c51836da277bba089a3409";
        const AGENT_TOKEN: &str = "fmac1_464d4143010005666f7267650004726f6f7400266f70733d726561642c77726974652c6272616e63682c6d657267652c6772616e742c7365616c00086f70733d72656164000e7265663d68656164732f6d61696e000b6167656e743d616c696365637145f038030df57b82d9daf2ecb9a5751371ebf6af656c156efd1afb218b4b";

        let key = [7u8; 32];

        let root = mint_root(&key).unwrap();
        assert_eq!(root.to_token(), ROOT_TOKEN, "root capability token drifted");

        let agent = attenuate_holder(
            &root,
            vec![
                "ops=read".into(),
                "ref=heads/main".into(),
                "agent=alice".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            agent.to_token(),
            AGENT_TOKEN,
            "attenuated capability token drifted"
        );

        // A pinned token must still verify against the root secret, so the test
        // fails loudly whether the drift is in signing or in verification.
        verify(&key, &Cap::from_token(ROOT_TOKEN).unwrap()).unwrap();
        verify(&key, &Cap::from_token(AGENT_TOKEN).unwrap()).unwrap();
    }
}
