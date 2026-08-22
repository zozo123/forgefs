use crate::cbor::{
    encode_array_header, encode_bytes, encode_map_sorted, encode_text, encode_u64, text_key, Reader,
};
use crate::object::{encode_file, parse_file};
use forge_types::{Error, ObjectId, ObjectType, Result};

const MAX_ITEMS: u64 = 100_000;
const MAX_PARENTS: u64 = 1_024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_AGENT_BYTES: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionRead {
    pub path: String,
    pub id: ObjectId,
}

/// Canonical v1 receipt payload. It records machine-verifiable contribution
/// boundaries only; orchestration, prompts, and wall-clock ordering remain out
/// of the trusted object model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    pub base: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub reads: Vec<ContributionRead>,
    pub writes: Vec<String>,
    pub agent: String,
    pub ts: u64,
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(Error::Invalid("contribution path bounds".into()));
    }
    Ok(())
}

fn validate_sorted_unique<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut prev: Option<&str> = None;
    for path in paths {
        validate_path(path)?;
        if let Some(prev) = prev {
            if prev.as_bytes() >= path.as_bytes() {
                return Err(Error::Invalid(
                    "contribution paths must be bytewise sorted unique".into(),
                ));
            }
        }
        prev = Some(path);
    }
    Ok(())
}

impl Contribution {
    fn validate(&self) -> Result<()> {
        if self.parents.len() as u64 > MAX_PARENTS
            || self.reads.len() as u64 > MAX_ITEMS
            || self.writes.len() as u64 > MAX_ITEMS
        {
            return Err(Error::Invalid("contribution fanout exceeds limit".into()));
        }
        if self.agent.is_empty() || self.agent.len() > MAX_AGENT_BYTES {
            return Err(Error::Invalid("contribution agent bounds".into()));
        }
        validate_sorted_unique(self.reads.iter().map(|r| r.path.as_str()))?;
        validate_sorted_unique(self.writes.iter().map(String::as_str))?;
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;

        let mut base_v = Vec::new();
        encode_bytes(&mut base_v, self.base.as_bytes());
        let mut tree_v = Vec::new();
        encode_bytes(&mut tree_v, self.tree.as_bytes());
        let mut parents_v = Vec::new();
        encode_array_header(&mut parents_v, self.parents.len() as u64);
        for parent in &self.parents {
            encode_bytes(&mut parents_v, parent.as_bytes());
        }
        let mut reads_v = Vec::new();
        encode_array_header(&mut reads_v, self.reads.len() as u64);
        for read in &self.reads {
            let mut path_v = Vec::new();
            encode_text(&mut path_v, &read.path);
            let mut id_v = Vec::new();
            encode_bytes(&mut id_v, read.id.as_bytes());
            encode_map_sorted(
                &mut reads_v,
                vec![(text_key("id"), id_v), (text_key("p"), path_v)],
            );
        }
        let mut writes_v = Vec::new();
        encode_array_header(&mut writes_v, self.writes.len() as u64);
        for path in &self.writes {
            encode_text(&mut writes_v, path);
        }
        let mut agent_v = Vec::new();
        encode_text(&mut agent_v, &self.agent);
        let mut ts_v = Vec::new();
        encode_u64(&mut ts_v, self.ts);

        let mut header = Vec::new();
        encode_map_sorted(
            &mut header,
            vec![
                (text_key("agent"), agent_v),
                (text_key("base"), base_v),
                (text_key("parents"), parents_v),
                (text_key("reads"), reads_v),
                (text_key("tree"), tree_v),
                (text_key("ts"), ts_v),
                (text_key("writes"), writes_v),
            ],
        );
        Ok(encode_file(ObjectType::Contribution, &header, &[]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (ty, header, payload) = parse_file(bytes)?;
        if ty != ObjectType::Contribution {
            return Err(Error::Corrupt("not a contribution".into()));
        }
        if !payload.is_empty() {
            return Err(Error::Corrupt("contribution has payload".into()));
        }
        let mut r = Reader::new(header);
        let n = r.map()?;
        let mut base = None;
        let mut tree = None;
        let mut parents = None;
        let mut reads = None;
        let mut writes = None;
        let mut agent = None;
        let mut ts = None;
        let mut last = None;
        for _ in 0..n {
            let key = r.text_map_key(&mut last)?;
            match key.as_str() {
                "base" => base = Some(ObjectId(r.bstr32()?)),
                "tree" => tree = Some(ObjectId(r.bstr32()?)),
                "parents" => {
                    let count = r.array()?;
                    if count > MAX_PARENTS {
                        return Err(Error::Corrupt(
                            "contribution parent count exceeds limit".into(),
                        ));
                    }
                    let mut out = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        out.push(ObjectId(r.bstr32()?));
                    }
                    parents = Some(out);
                }
                "reads" => {
                    let count = r.array()?;
                    if count > MAX_ITEMS {
                        return Err(Error::Corrupt(
                            "contribution read count exceeds limit".into(),
                        ));
                    }
                    let mut out = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        let fields = r.map()?;
                        let mut path = None;
                        let mut id = None;
                        let mut last_read = None;
                        for _ in 0..fields {
                            let key = r.text_map_key(&mut last_read)?;
                            match key.as_str() {
                                "p" => path = Some(r.text()?),
                                "id" => id = Some(ObjectId(r.bstr32()?)),
                                _ => {
                                    return Err(Error::Corrupt(format!(
                                        "unknown contribution read key {key}"
                                    )))
                                }
                            }
                        }
                        out.push(ContributionRead {
                            path: path
                                .ok_or_else(|| Error::Corrupt("contribution read path".into()))?,
                            id: id.ok_or_else(|| Error::Corrupt("contribution read id".into()))?,
                        });
                    }
                    reads = Some(out);
                }
                "writes" => {
                    let count = r.array()?;
                    if count > MAX_ITEMS {
                        return Err(Error::Corrupt(
                            "contribution write count exceeds limit".into(),
                        ));
                    }
                    let mut out = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        out.push(r.text()?);
                    }
                    writes = Some(out);
                }
                "agent" => agent = Some(r.text()?),
                "ts" => ts = Some(r.u64()?),
                _ => return Err(Error::Corrupt(format!("unknown contribution key {key}"))),
            }
        }
        if !r.at_end() {
            return Err(Error::Corrupt("contribution header trailing bytes".into()));
        }
        let value = Contribution {
            base: base.ok_or_else(|| Error::Corrupt("contribution base".into()))?,
            tree: tree.ok_or_else(|| Error::Corrupt("contribution tree".into()))?,
            parents: parents.ok_or_else(|| Error::Corrupt("contribution parents".into()))?,
            reads: reads.ok_or_else(|| Error::Corrupt("contribution reads".into()))?,
            writes: writes.ok_or_else(|| Error::Corrupt("contribution writes".into()))?,
            agent: agent.ok_or_else(|| Error::Corrupt("contribution agent".into()))?,
            ts: ts.ok_or_else(|| Error::Corrupt("contribution ts".into()))?,
        };
        value
            .validate()
            .map_err(|e| Error::Corrupt(e.to_string()))?;
        if value.encode()? != bytes {
            return Err(Error::Corrupt("non-canonical contribution encoding".into()));
        }
        Ok(value)
    }
}
