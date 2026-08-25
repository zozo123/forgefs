//! Typed objects and the on-disk file layout.

use crate::cbor::{
    encode_array_header, encode_bool, encode_bytes, encode_map_sorted, encode_null, encode_text,
    encode_u64, text_key, Reader,
};
use crate::tree::{validate_name, Tree, TreeEntry};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};

const MAX_TREE_ENTRIES: u64 = 100_000;
const MAX_COMMIT_PARENTS: u64 = 1_024;
const MAX_CONFLICT_ITEMS: u64 = 100_000;

pub fn hash_bytes(bytes: &[u8]) -> ObjectId {
    ObjectId(*blake3::hash(bytes).as_bytes())
}

/// Identity of the object file formed by concatenating `parts`, without
/// materializing that concatenation. `hash_parts(&[a, b]) == hash_bytes(&[a,
/// b].concat())` by construction, so a streaming publisher computes the same
/// ObjectId as a buffering one (I2).
pub fn hash_parts(parts: &[&[u8]]) -> ObjectId {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p);
    }
    ObjectId(*h.finalize().as_bytes())
}

/// The framed prefix that precedes a Blob payload: type byte, header length,
/// and the canonical `{size}` header. `blob_frame_prefix(n) ++ data` is
/// byte-for-byte `Blob { data }.encode()` when `data.len() == n`, which lets a
/// publisher hash and write a payload it does not own a second copy of. The
/// encoding is unchanged; this only exposes the split point (FORMAT.md v1).
pub fn blob_frame_prefix(size: u64) -> Vec<u8> {
    let mut header = Vec::new();
    let mut size_v = Vec::new();
    encode_u64(&mut size_v, size);
    encode_map_sorted(&mut header, vec![(text_key("size"), size_v)]);
    let n = u32::try_from(header.len()).expect("Forge object header exceeds u32::MAX");
    let mut v = Vec::with_capacity(5 + header.len());
    v.push(ObjectType::Blob as u8);
    v.extend_from_slice(&n.to_be_bytes());
    v.extend_from_slice(&header);
    v
}

pub fn encode_file(ty: ObjectType, header: &[u8], payload: &[u8]) -> Vec<u8> {
    let n = u32::try_from(header.len()).expect("Forge object header exceeds u32::MAX");
    let mut v = Vec::with_capacity(5 + header.len() + payload.len());
    v.push(ty as u8);
    v.extend_from_slice(&n.to_be_bytes());
    v.extend_from_slice(header);
    v.extend_from_slice(payload);
    v
}

pub fn parse_file(bytes: &[u8]) -> Result<(ObjectType, &[u8], &[u8])> {
    if bytes.len() < 5 {
        return Err(Error::Corrupt("object file too short".into()));
    }
    let ty = ObjectType::from_u8(bytes[0])?;
    let n = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    let header_end = 5usize
        .checked_add(n)
        .ok_or_else(|| Error::Corrupt("object header length overflow".into()))?;
    if header_end > bytes.len() {
        return Err(Error::Corrupt("object header truncated".into()));
    }
    let header = &bytes[5..header_end];
    let payload = &bytes[header_end..];
    Ok((ty, header, payload))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub data: Vec<u8>,
}

impl Blob {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = blob_frame_prefix(self.data.len() as u64);
        v.reserve_exact(self.data.len());
        v.extend_from_slice(&self.data);
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Blob {
            return Err(Error::Corrupt("not a blob".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut size = None;
        let mut last = None;
        for _ in 0..n {
            let k = r.text_map_key(&mut last)?;
            match k.as_str() {
                "size" => size = Some(r.u64()?),
                _ => return Err(Error::Corrupt(format!("unknown blob key {k}"))),
            }
        }
        let size = size.ok_or_else(|| Error::Corrupt("blob missing size".into()))?;
        let size =
            usize::try_from(size).map_err(|_| Error::Corrupt("blob size overflow".into()))?;
        if size != payload.len() {
            return Err(Error::Corrupt(format!(
                "blob size {size} != payload {}",
                payload.len()
            )));
        }
        if !r.at_end() {
            return Err(Error::Corrupt("blob header trailing bytes".into()));
        }
        Ok(Blob {
            data: payload.to_vec(),
        })
    }
}

impl Tree {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut entries_cbor = Vec::new();
        encode_array_header(&mut entries_cbor, self.entries.len() as u64);
        for e in &self.entries {
            validate_name(&e.name)?;
            let mut n_v = Vec::new();
            encode_text(&mut n_v, &e.name);
            let mut k_v = Vec::new();
            encode_u64(&mut k_v, e.kind as u64);
            let mut id_v = Vec::new();
            encode_bytes(&mut id_v, e.id.as_bytes());
            let mut x_v = Vec::new();
            encode_bool(&mut x_v, e.exec);
            encode_map_sorted(
                &mut entries_cbor,
                vec![
                    (text_key("id"), id_v),
                    (text_key("k"), k_v),
                    (text_key("n"), n_v),
                    (text_key("x"), x_v),
                ],
            );
        }
        let mut header = Vec::new();
        encode_map_sorted(&mut header, vec![(text_key("e"), entries_cbor)]);
        Ok(encode_file(ObjectType::Tree, &header, &[]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Tree {
            return Err(Error::Corrupt("not a tree".into()));
        }
        if !payload.is_empty() {
            return Err(Error::Corrupt("tree has payload".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut entries = None;
        let mut last_top: Option<Vec<u8>> = None;
        for _ in 0..n {
            let k = r.text_map_key(&mut last_top)?;
            if k != "e" {
                return Err(Error::Corrupt(format!("unknown tree key {k}")));
            }
            let m = r.array()?;
            if m > MAX_TREE_ENTRIES {
                return Err(Error::Corrupt("tree fanout exceeds limit".into()));
            }
            let mut v = Vec::with_capacity(m as usize);
            for _ in 0..m {
                let kn = r.map()?;
                let mut name = None;
                let mut kind = None;
                let mut id = None;
                let mut exec = None;
                let mut last_ent: Option<Vec<u8>> = None;
                for _ in 0..kn {
                    let fk = r.text_map_key(&mut last_ent)?;
                    match fk.as_str() {
                        "n" => name = Some(r.text()?),
                        "k" => {
                            let raw = r.u64()?;
                            let raw = u8::try_from(raw)
                                .map_err(|_| Error::Corrupt("entry kind overflow".into()))?;
                            kind = Some(EntryKind::from_u8(raw)?);
                        }
                        "id" => id = Some(ObjectId(r.bstr32()?)),
                        "x" => exec = Some(r.bool()?),
                        _ => return Err(Error::Corrupt(format!("unknown entry key {fk}"))),
                    }
                }
                v.push(TreeEntry {
                    name: name.ok_or_else(|| Error::Corrupt("entry n".into()))?,
                    kind: kind.ok_or_else(|| Error::Corrupt("entry k".into()))?,
                    id: id.ok_or_else(|| Error::Corrupt("entry id".into()))?,
                    exec: exec.ok_or_else(|| Error::Corrupt("entry x".into()))?,
                });
            }
            entries = Some(v);
        }
        if !r.at_end() {
            return Err(Error::Corrupt("tree header trailing bytes".into()));
        }
        Tree::from_canonical(entries.ok_or_else(|| Error::Corrupt("tree missing e".into()))?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub agent: String,
    pub msg: String,
    pub ts: u64,
    pub landmark: bool,
    pub contrib: Option<ObjectId>,
}

impl Commit {
    pub fn encode(&self) -> Vec<u8> {
        let mut tree_v = Vec::new();
        encode_bytes(&mut tree_v, self.tree.as_bytes());
        let mut parents_v = Vec::new();
        encode_array_header(&mut parents_v, self.parents.len() as u64);
        for p in &self.parents {
            encode_bytes(&mut parents_v, p.as_bytes());
        }
        let mut agent_v = Vec::new();
        encode_text(&mut agent_v, &self.agent);
        let mut msg_v = Vec::new();
        encode_text(&mut msg_v, &self.msg);
        let mut ts_v = Vec::new();
        encode_u64(&mut ts_v, self.ts);
        let mut lm_v = Vec::new();
        encode_bool(&mut lm_v, self.landmark);
        let mut fields = vec![
            (text_key("agent"), agent_v),
            (text_key("lm"), lm_v),
            (text_key("msg"), msg_v),
            (text_key("parents"), parents_v),
            (text_key("tree"), tree_v),
            (text_key("ts"), ts_v),
        ];
        if let Some(contrib) = self.contrib {
            let mut contrib_v = Vec::new();
            encode_bytes(&mut contrib_v, contrib.as_bytes());
            fields.push((text_key("contrib"), contrib_v));
        }
        let mut header = Vec::new();
        encode_map_sorted(&mut header, fields);
        encode_file(ObjectType::Commit, &header, &[])
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Commit {
            return Err(Error::Corrupt("not a commit".into()));
        }
        if !payload.is_empty() {
            return Err(Error::Corrupt("commit has payload".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut tree = None;
        let mut parents = None;
        let mut agent = None;
        let mut msg = None;
        let mut ts = None;
        let mut lm = None;
        let mut contrib = None;
        let mut last = None;
        for _ in 0..n {
            let k = r.text_map_key(&mut last)?;
            match k.as_str() {
                "tree" => tree = Some(ObjectId(r.bstr32()?)),
                "parents" => {
                    let m = r.array()?;
                    if m > MAX_COMMIT_PARENTS {
                        return Err(Error::Corrupt("commit parent count exceeds limit".into()));
                    }
                    let mut v = Vec::with_capacity(m as usize);
                    for _ in 0..m {
                        v.push(ObjectId(r.bstr32()?));
                    }
                    parents = Some(v);
                }
                "agent" => agent = Some(r.text()?),
                "msg" => msg = Some(r.text()?),
                "ts" => ts = Some(r.u64()?),
                "lm" => lm = Some(r.bool()?),
                "contrib" => contrib = Some(ObjectId(r.bstr32()?)),
                _ => return Err(Error::Corrupt(format!("unknown commit key {k}"))),
            }
        }
        if !r.at_end() {
            return Err(Error::Corrupt("commit header trailing bytes".into()));
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| Error::Corrupt("commit tree".into()))?,
            parents: parents.ok_or_else(|| Error::Corrupt("commit parents".into()))?,
            agent: agent.ok_or_else(|| Error::Corrupt("commit agent".into()))?,
            msg: msg.ok_or_else(|| Error::Corrupt("commit msg".into()))?,
            ts: ts.ok_or_else(|| Error::Corrupt("commit ts".into()))?,
            landmark: lm.ok_or_else(|| Error::Corrupt("commit lm".into()))?,
            contrib,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictPath {
    pub path: String,
    pub a: Option<ObjectId>,
    pub b: Option<ObjectId>,
    pub base: Option<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub bases: Vec<ObjectId>,
    pub ours: ObjectId,
    pub theirs: ObjectId,
    pub paths: Vec<ConflictPath>,
    pub causal: Vec<ObjectId>,
}

impl Conflict {
    pub fn encode(&self) -> Vec<u8> {
        let mut bases_v = Vec::new();
        encode_array_header(&mut bases_v, self.bases.len() as u64);
        for id in &self.bases {
            encode_bytes(&mut bases_v, id.as_bytes());
        }
        let mut ours_v = Vec::new();
        encode_bytes(&mut ours_v, self.ours.as_bytes());
        let mut theirs_v = Vec::new();
        encode_bytes(&mut theirs_v, self.theirs.as_bytes());
        let mut paths_v = Vec::new();
        encode_array_header(&mut paths_v, self.paths.len() as u64);
        for p in &self.paths {
            let mut p_v = Vec::new();
            encode_text(&mut p_v, &p.path);
            let mut a_v = Vec::new();
            opt_id(&mut a_v, p.a);
            let mut b_v = Vec::new();
            opt_id(&mut b_v, p.b);
            let mut base_v = Vec::new();
            opt_id(&mut base_v, p.base);
            encode_map_sorted(
                &mut paths_v,
                vec![
                    (text_key("a"), a_v),
                    (text_key("b"), b_v),
                    (text_key("base"), base_v),
                    (text_key("p"), p_v),
                ],
            );
        }
        let mut causal_v = Vec::new();
        encode_array_header(&mut causal_v, self.causal.len() as u64);
        for id in &self.causal {
            encode_bytes(&mut causal_v, id.as_bytes());
        }
        let mut header = Vec::new();
        encode_map_sorted(
            &mut header,
            vec![
                (text_key("bases"), bases_v),
                (text_key("causal"), causal_v),
                (text_key("ours"), ours_v),
                (text_key("paths"), paths_v),
                (text_key("theirs"), theirs_v),
            ],
        );
        encode_file(ObjectType::Conflict, &header, &[])
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Conflict {
            return Err(Error::Corrupt("not a conflict".into()));
        }
        if !payload.is_empty() {
            return Err(Error::Corrupt("conflict has payload".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut bases = None;
        let mut ours = None;
        let mut theirs = None;
        let mut paths = None;
        let mut causal = None;
        let mut last = None;
        for _ in 0..n {
            let k = r.text_map_key(&mut last)?;
            match k.as_str() {
                "bases" => bases = Some(id_array(&mut r)?),
                "ours" => ours = Some(ObjectId(r.bstr32()?)),
                "theirs" => theirs = Some(ObjectId(r.bstr32()?)),
                "causal" => causal = Some(id_array(&mut r)?),
                "paths" => {
                    let m = r.array()?;
                    if m > MAX_CONFLICT_ITEMS {
                        return Err(Error::Corrupt("conflict path count exceeds limit".into()));
                    }
                    let mut v = Vec::with_capacity(m as usize);
                    for _ in 0..m {
                        let kn = r.map()?;
                        let mut path = None;
                        let mut a = None;
                        let mut b = None;
                        let mut base = None;
                        let mut last_path = None;
                        for _ in 0..kn {
                            let fk = r.text_map_key(&mut last_path)?;
                            match fk.as_str() {
                                "p" => path = Some(r.text()?),
                                "a" => a = r.null_or(|r| Ok(ObjectId(r.bstr32()?)))?,
                                "b" => b = r.null_or(|r| Ok(ObjectId(r.bstr32()?)))?,
                                "base" => base = r.null_or(|r| Ok(ObjectId(r.bstr32()?)))?,
                                _ => return Err(Error::Corrupt(format!("path key {fk}"))),
                            }
                        }
                        v.push(ConflictPath {
                            path: path.ok_or_else(|| Error::Corrupt("conflict path".into()))?,
                            a,
                            b,
                            base,
                        });
                    }
                    paths = Some(v);
                }
                _ => return Err(Error::Corrupt(format!("unknown conflict key {k}"))),
            }
        }
        if !r.at_end() {
            return Err(Error::Corrupt("conflict header trailing bytes".into()));
        }
        Ok(Conflict {
            bases: bases.unwrap_or_default(),
            ours: ours.ok_or_else(|| Error::Corrupt("conflict ours".into()))?,
            theirs: theirs.ok_or_else(|| Error::Corrupt("conflict theirs".into()))?,
            paths: paths.unwrap_or_default(),
            causal: causal.unwrap_or_default(),
        })
    }
}

fn opt_id(out: &mut Vec<u8>, id: Option<ObjectId>) {
    match id {
        Some(id) => encode_bytes(out, id.as_bytes()),
        None => encode_null(out),
    }
}

fn id_array(r: &mut Reader<'_>) -> Result<Vec<ObjectId>> {
    let m = r.array()?;
    if m > MAX_CONFLICT_ITEMS {
        return Err(Error::Corrupt("object id array exceeds limit".into()));
    }
    let mut v = Vec::with_capacity(m as usize);
    for _ in 0..m {
        v.push(ObjectId(r.bstr32()?));
    }
    Ok(v)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub tree: ObjectId,
    pub commit: ObjectId,
    pub tag: String,
    pub ts: u64,
    pub prov: ObjectId,
    pub pk: [u8; 32],
    pub sig: [u8; 64],
}

impl Snapshot {
    pub fn encode_unsigned(&self) -> Vec<u8> {
        let mut s = self.clone();
        s.sig = [0u8; 64];
        s.encode()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut tree_v = Vec::new();
        encode_bytes(&mut tree_v, self.tree.as_bytes());
        let mut commit_v = Vec::new();
        encode_bytes(&mut commit_v, self.commit.as_bytes());
        let mut tag_v = Vec::new();
        encode_text(&mut tag_v, &self.tag);
        let mut ts_v = Vec::new();
        encode_u64(&mut ts_v, self.ts);
        let mut prov_v = Vec::new();
        encode_bytes(&mut prov_v, self.prov.as_bytes());
        let mut pk_v = Vec::new();
        encode_bytes(&mut pk_v, &self.pk);
        let mut sig_v = Vec::new();
        encode_bytes(&mut sig_v, &self.sig);
        let mut header = Vec::new();
        encode_map_sorted(
            &mut header,
            vec![
                (text_key("commit"), commit_v),
                (text_key("pk"), pk_v),
                (text_key("prov"), prov_v),
                (text_key("sig"), sig_v),
                (text_key("tag"), tag_v),
                (text_key("tree"), tree_v),
                (text_key("ts"), ts_v),
            ],
        );
        encode_file(ObjectType::Snapshot, &header, &[])
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Snapshot {
            return Err(Error::Corrupt("not a snapshot".into()));
        }
        if !payload.is_empty() {
            return Err(Error::Corrupt("snapshot has payload".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut tree = None;
        let mut commit = None;
        let mut tag = None;
        let mut ts = None;
        let mut prov = None;
        let mut pk = None;
        let mut sig = None;
        let mut last = None;
        for _ in 0..n {
            let k = r.text_map_key(&mut last)?;
            match k.as_str() {
                "tree" => tree = Some(ObjectId(r.bstr32()?)),
                "commit" => commit = Some(ObjectId(r.bstr32()?)),
                "tag" => tag = Some(r.text()?),
                "ts" => ts = Some(r.u64()?),
                "prov" => prov = Some(ObjectId(r.bstr32()?)),
                "pk" => {
                    let b = r.bytes()?;
                    if b.len() != 32 {
                        return Err(Error::Corrupt("pk len".into()));
                    }
                    let mut a = [0u8; 32];
                    a.copy_from_slice(b);
                    pk = Some(a);
                }
                "sig" => {
                    let b = r.bytes()?;
                    if b.len() != 64 {
                        return Err(Error::Corrupt("sig len".into()));
                    }
                    let mut a = [0u8; 64];
                    a.copy_from_slice(b);
                    sig = Some(a);
                }
                _ => return Err(Error::Corrupt(format!("unknown snapshot key {k}"))),
            }
        }
        if !r.at_end() {
            return Err(Error::Corrupt("snapshot header trailing bytes".into()));
        }
        Ok(Snapshot {
            tree: tree.ok_or_else(|| Error::Corrupt("snap tree".into()))?,
            commit: commit.ok_or_else(|| Error::Corrupt("snap commit".into()))?,
            tag: tag.ok_or_else(|| Error::Corrupt("snap tag".into()))?,
            ts: ts.ok_or_else(|| Error::Corrupt("snap ts".into()))?,
            prov: prov.ok_or_else(|| Error::Corrupt("snap prov".into()))?,
            pk: pk.ok_or_else(|| Error::Corrupt("snap pk".into()))?,
            sig: sig.ok_or_else(|| Error::Corrupt("snap sig".into()))?,
        })
    }
}

pub fn decode_object_type(bytes: &[u8]) -> Result<ObjectType> {
    if bytes.is_empty() {
        return Err(Error::Corrupt("empty object".into()));
    }
    ObjectType::from_u8(bytes[0])
}
