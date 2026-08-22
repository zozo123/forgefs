#!/usr/bin/env bash
set -euo pipefail

python3 - <<'PY'
from pathlib import Path

p = Path('crates/forge-types/src/lib.rs')
s = p.read_text()
assert '    Snapshot = 0x05,\n}' in s
s = s.replace('    Snapshot = 0x05,\n}', '    Snapshot = 0x05,\n    Contribution = 0x06,\n}')
assert '            0x05 => Ok(Self::Snapshot),\n' in s
s = s.replace('            0x05 => Ok(Self::Snapshot),\n', '            0x05 => Ok(Self::Snapshot),\n            0x06 => Ok(Self::Contribution),\n')
assert '            Self::Snapshot => "snapshot",\n' in s
s = s.replace('            Self::Snapshot => "snapshot",\n', '            Self::Snapshot => "snapshot",\n            Self::Contribution => "contribution",\n')
p.write_text(s)

p = Path('crates/forge-core/src/lib.rs')
s = p.read_text()
assert 'pub mod cbor;\n' in s
s = s.replace('pub mod cbor;\n', 'pub mod cbor;\npub mod contribution;\n')
assert 'pub use object::{' in s
s = s.replace('pub use object::{', 'pub use contribution::{Contribution, ContributionRead};\npub use object::{')
p.write_text(s)

p = Path('fuzz/fuzz_targets/object_decode.rs')
s = p.read_text()
assert 'use forge_core::{parse_file, Blob, Commit, Conflict, Snapshot, Tree};' in s
s = s.replace(
    'use forge_core::{parse_file, Blob, Commit, Conflict, Snapshot, Tree};',
    'use forge_core::{parse_file, Blob, Commit, Conflict, Contribution, Snapshot, Tree};',
)
assert '    let _ = Conflict::decode(data);\n' in s
s = s.replace('    let _ = Conflict::decode(data);\n', '    let _ = Conflict::decode(data);\n    let _ = Contribution::decode(data);\n')
p.write_text(s)
PY

cat > crates/forge-core/src/contribution.rs <<'RS'
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
                            path: path.ok_or_else(|| {
                                Error::Corrupt("contribution read path".into())
                            })?,
                            id: id
                                .ok_or_else(|| Error::Corrupt("contribution read id".into()))?,
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
            return Err(Error::Corrupt(
                "contribution header trailing bytes".into(),
            ));
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
            return Err(Error::Corrupt(
                "non-canonical contribution encoding".into(),
            ));
        }
        Ok(value)
    }
}
RS

cat > crates/forge-core/tests/contribution.rs <<'RS'
use forge_core::{hash_bytes, Blob, Contribution, ContributionRead};
use forge_types::ObjectId;

fn sample() -> Contribution {
    Contribution {
        base: ObjectId([0x10; 32]),
        tree: ObjectId([0x20; 32]),
        parents: vec![ObjectId([0x30; 32])],
        reads: vec![
            ContributionRead {
                path: "Cargo.lock".into(),
                id: ObjectId([0x40; 32]),
            },
            ContributionRead {
                path: "src/lib.rs".into(),
                id: ObjectId([0x41; 32]),
            },
        ],
        writes: vec!["README.md".into(), "src/lib.rs".into()],
        agent: "agent-a".into(),
        ts: 1,
    }
}

#[test]
fn contribution_roundtrip_and_golden_identity() {
    let value = sample();
    let bytes = value.encode().unwrap();
    assert_eq!(Contribution::decode(&bytes).unwrap(), value);
    assert_eq!(
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        include_str!("../../../testdata/canonical/contribution.hex").trim()
    );
    assert_eq!(
        hash_bytes(&bytes).hex(),
        include_str!("../../../testdata/canonical/contribution.oid").trim()
    );
}

#[test]
fn contribution_requires_bytewise_sorted_unique_sets() {
    let mut value = sample();
    value.writes.reverse();
    assert!(value.encode().is_err());

    let mut value = sample();
    value.reads[1].path = value.reads[0].path.clone();
    assert!(value.encode().is_err());
}

#[test]
fn contribution_decoder_is_type_strict() {
    let blob = Blob { data: b"x".to_vec() }.encode();
    assert!(Contribution::decode(&blob).is_err());
}
RS

mkdir -p crates/forge-core/examples testdata/canonical
cat > crates/forge-core/examples/generate_contribution_golden.rs <<'RS'
use forge_core::{hash_bytes, Contribution, ContributionRead};
use forge_types::ObjectId;

fn main() {
    let value = Contribution {
        base: ObjectId([0x10; 32]),
        tree: ObjectId([0x20; 32]),
        parents: vec![ObjectId([0x30; 32])],
        reads: vec![
            ContributionRead { path: "Cargo.lock".into(), id: ObjectId([0x40; 32]) },
            ContributionRead { path: "src/lib.rs".into(), id: ObjectId([0x41; 32]) },
        ],
        writes: vec!["README.md".into(), "src/lib.rs".into()],
        agent: "agent-a".into(),
        ts: 1,
    };
    let bytes = value.encode().unwrap();
    println!("{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    eprintln!("{}", hash_bytes(&bytes));
}
RS

cargo fmt --all
cargo run --locked --quiet -p forge-core --example generate_contribution_golden > /tmp/contribution.hex 2> /tmp/contribution.oid
tr -d '\n' < /tmp/contribution.hex > testdata/canonical/contribution.hex
printf '\n' >> testdata/canonical/contribution.hex
tail -n 1 /tmp/contribution.oid | tr -d '\n' > testdata/canonical/contribution.oid
printf '\n' >> testdata/canonical/contribution.oid
rm crates/forge-core/examples/generate_contribution_golden.rs
rmdir crates/forge-core/examples || true

cargo test --locked -p forge-core --all-targets
cargo check --locked --workspace --all-targets
cargo check --manifest-path fuzz/Cargo.toml --bins

git rm -f .github/workflows/autopatch-contribution-108.yml .github/worker-trigger-108 .github/swarm-worker.sh
git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git add crates/forge-types/src/lib.rs crates/forge-core/src/lib.rs crates/forge-core/src/contribution.rs crates/forge-core/tests/contribution.rs fuzz/fuzz_targets/object_decode.rs testdata/canonical/contribution.hex testdata/canonical/contribution.oid
git commit -m 'object: add canonical Contribution type 0x06 (#108)'
git push origin HEAD:objects/contribution-108
