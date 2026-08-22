#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main objects/contribution-108-v3
git merge --no-edit origin/main

python3 - <<'PY'
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if text.count(old) != 1:
        raise SystemExit(f"{label}: expected exactly one marker, found {text.count(old)}")
    return text.replace(old, new, 1)

# 1) Bound Contribution receipts before expensive decode/encode work.
p = Path("crates/forge-core/src/contribution.rs")
s = p.read_text()
if "MAX_SERIALIZED_HEADER_BYTES" not in s:
    s = replace_once(
        s,
        "const MAX_AGENT_BYTES: usize = 1_024;\n",
        "const MAX_AGENT_BYTES: usize = 1_024;\nconst MAX_SERIALIZED_HEADER_BYTES: usize = 8 * 1024 * 1024;\n",
        "contribution size constant",
    )
    s = replace_once(
        s,
        "        if self.agent.is_empty() || self.agent.len() > MAX_AGENT_BYTES {\n            return Err(Error::Invalid(\"contribution agent bounds\".into()));\n        }\n        validate_sorted_unique(self.reads.iter().map(|r| r.path.as_str()))?;\n",
        "        if self.agent.is_empty() || self.agent.len() > MAX_AGENT_BYTES {\n            return Err(Error::Invalid(\"contribution agent bounds\".into()));\n        }\n\n        // A receipt is control-plane metadata, never an unbounded data carrier.\n        // This conservative preflight upper-bounds CBOR framing so encode cannot\n        // allocate hundreds of MiB before discovering an oversized header.\n        let mut estimated = 1_024usize\n            .checked_add(self.agent.len())\n            .and_then(|n| n.checked_add(self.parents.len().saturating_mul(40)))\n            .ok_or_else(|| Error::Invalid(\"contribution size overflow\".into()))?;\n        for read in &self.reads {\n            estimated = estimated\n                .checked_add(read.path.len().saturating_add(64))\n                .ok_or_else(|| Error::Invalid(\"contribution size overflow\".into()))?;\n        }\n        for path in &self.writes {\n            estimated = estimated\n                .checked_add(path.len().saturating_add(8))\n                .ok_or_else(|| Error::Invalid(\"contribution size overflow\".into()))?;\n        }\n        if estimated > MAX_SERIALIZED_HEADER_BYTES {\n            return Err(Error::Invalid(\"contribution exceeds serialized size limit\".into()));\n        }\n\n        validate_sorted_unique(self.reads.iter().map(|r| r.path.as_str()))?;\n",
        "contribution preflight",
    )
    s = replace_once(
        s,
        "        encode_map_sorted(\n            &mut header,\n            vec![\n                (text_key(\"agent\"), agent_v),\n                (text_key(\"base\"), base_v),\n                (text_key(\"parents\"), parents_v),\n                (text_key(\"reads\"), reads_v),\n                (text_key(\"tree\"), tree_v),\n                (text_key(\"ts\"), ts_v),\n                (text_key(\"writes\"), writes_v),\n            ],\n        );\n        Ok(encode_file(ObjectType::Contribution, &header, &[]))\n",
        "        encode_map_sorted(\n            &mut header,\n            vec![\n                (text_key(\"agent\"), agent_v),\n                (text_key(\"base\"), base_v),\n                (text_key(\"parents\"), parents_v),\n                (text_key(\"reads\"), reads_v),\n                (text_key(\"tree\"), tree_v),\n                (text_key(\"ts\"), ts_v),\n                (text_key(\"writes\"), writes_v),\n            ],\n        );\n        if header.len() > MAX_SERIALIZED_HEADER_BYTES {\n            return Err(Error::Invalid(\"contribution exceeds serialized size limit\".into()));\n        }\n        Ok(encode_file(ObjectType::Contribution, &header, &[]))\n",
        "contribution encoded limit",
    )
    s = replace_once(
        s,
        "    pub fn decode(bytes: &[u8]) -> Result<Self> {\n        let (ty, header, payload) = parse_file(bytes)?;\n",
        "    pub fn decode(bytes: &[u8]) -> Result<Self> {\n        if bytes.len() > MAX_SERIALIZED_HEADER_BYTES.saturating_add(5) {\n            return Err(Error::Corrupt(\"contribution exceeds serialized size limit\".into()));\n        }\n        let (ty, header, payload) = parse_file(bytes)?;\n        if header.len() > MAX_SERIALIZED_HEADER_BYTES {\n            return Err(Error::Corrupt(\"contribution exceeds serialized size limit\".into()));\n        }\n",
        "contribution decode limit",
    )
    s += r'''

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn serialized_limit_rejects_only_over_boundary_before_decode() {
        let at_limit = vec![0u8; MAX_SERIALIZED_HEADER_BYTES + 5];
        let at_err = Contribution::decode(&at_limit).unwrap_err().to_string();
        assert!(!at_err.contains("serialized size limit"));

        let over_limit = vec![0u8; MAX_SERIALIZED_HEADER_BYTES + 6];
        let over_err = Contribution::decode(&over_limit).unwrap_err().to_string();
        assert!(over_err.contains("serialized size limit"));
    }

    #[test]
    fn encode_preflight_rejects_pathological_receipt() {
        let writes = (0..2_100)
            .map(|i| format!("{i:06}{}", "a".repeat(4_090)))
            .collect();
        let value = Contribution {
            base: ObjectId([0; 32]),
            tree: ObjectId([1; 32]),
            parents: Vec::new(),
            reads: Vec::new(),
            writes,
            agent: "agent".into(),
            ts: 0,
        };
        let err = value.encode().unwrap_err().to_string();
        assert!(err.contains("serialized size limit"));
    }
}
'''
p.write_text(s)

# 2) Commit may optionally point at its Contribution. Omitting the field must
# preserve every legacy commit byte-for-byte / ObjectId.
p = Path("crates/forge-core/src/object.rs")
s = p.read_text()
if "pub contrib: Option<ObjectId>" not in s:
    s = replace_once(
        s,
        "    pub landmark: bool,\n}\n\nimpl Commit {",
        "    pub landmark: bool,\n    pub contrib: Option<ObjectId>,\n}\n\nimpl Commit {",
        "commit struct contrib",
    )
    old = '''        let mut header = Vec::new();
        encode_map_sorted(
            &mut header,
            vec![
                (text_key("agent"), agent_v),
                (text_key("lm"), lm_v),
                (text_key("msg"), msg_v),
                (text_key("parents"), parents_v),
                (text_key("tree"), tree_v),
                (text_key("ts"), ts_v),
            ],
        );
        encode_file(ObjectType::Commit, &header, &[])
'''
    new = '''        let mut fields = vec![
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
'''
    s = replace_once(s, old, new, "commit encode contrib")
    s = replace_once(
        s,
        "        let mut lm = None;\n        let mut last = None;",
        "        let mut lm = None;\n        let mut contrib = None;\n        let mut last = None;",
        "commit decode contrib var",
    )
    s = replace_once(
        s,
        '                "lm" => lm = Some(r.bool()?),\n                _ => return Err(Error::Corrupt(format!("unknown commit key {k}"))),',
        '                "lm" => lm = Some(r.bool()?),\n                "contrib" => contrib = Some(ObjectId(r.bstr32()?)),\n                _ => return Err(Error::Corrupt(format!("unknown commit key {k}"))),',
        "commit decode contrib key",
    )
    s = replace_once(
        s,
        "            landmark: lm.ok_or_else(|| Error::Corrupt(\"commit lm\".into()))?,\n        })",
        "            landmark: lm.ok_or_else(|| Error::Corrupt(\"commit lm\".into()))?,\n            contrib,\n        })",
        "commit decode contrib result",
    )
p.write_text(s)

# Add contrib: None to every existing Commit literal outside object.rs. This is
# deliberately syntax-local and fails if a literal is single-line or malformed.
def patch_commit_literals(path: Path) -> None:
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    out = []
    i = 0
    changed = False
    while i < len(lines):
        line = lines[i]
        if "Commit {" not in line or "struct Commit {" in line or "impl Commit {" in line:
            out.append(line)
            i += 1
            continue
        block = [line]
        depth = line.count("{") - line.count("}")
        i += 1
        while depth > 0 and i < len(lines):
            block.append(lines[i])
            depth += lines[i].count("{") - lines[i].count("}")
            i += 1
        if depth != 0:
            raise SystemExit(f"unbalanced Commit literal in {path}")
        joined = "".join(block)
        if "contrib:" not in joined:
            if len(block) == 1:
                raise SystemExit(f"single-line Commit literal requires explicit handling in {path}")
            closing = block[-1]
            indent = len(closing) - len(closing.lstrip(" "))
            block.insert(-1, " " * (indent + 4) + "contrib: None,\n")
            changed = True
        out.extend(block)
    if changed:
        path.write_text("".join(out))

for path in Path("crates").rglob("*.rs"):
    if path.as_posix() != "crates/forge-core/src/object.rs":
        patch_commit_literals(path)

# 3) Reachability must follow Commit -> Contribution.
p = Path("crates/forge-api/src/fsck.rs")
s = p.read_text()
if "format!(\"commit:{id}:contribution\")" not in s:
    marker = '''                for parent in commit.parents {
                    queue.push_back((
                        parent,
                        Some(ObjectType::Commit),
                        format!("commit:{id}:parent"),
                    ));
                }
'''
    replacement = marker + '''                if let Some(contrib) = commit.contrib {
                    queue.push_back((
                        contrib,
                        Some(ObjectType::Contribution),
                        format!("commit:{id}:contribution"),
                    ));
                }
'''
    s = replace_once(s, marker, replacement, "fsck commit contribution edge")
p.write_text(s)

# Explicit regression: None preserves existing golden vectors; Some round-trips.
p = Path("crates/forge-core/tests/contribution.rs")
s = p.read_text()
if "commit_contribution_link_roundtrips" not in s:
    s += r'''

#[test]
fn commit_contribution_link_roundtrips() {
    use forge_core::Commit;

    let contrib = ObjectId([9; 32]);
    let commit = Commit {
        tree: ObjectId([1; 32]),
        parents: vec![ObjectId([2; 32])],
        agent: "agent".into(),
        msg: "msg".into(),
        ts: 7,
        landmark: false,
        contrib: Some(contrib),
    };
    let decoded = Commit::decode(&commit.encode()).unwrap();
    assert_eq!(decoded.contrib, Some(contrib));
}
'''
p.write_text(s)
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked

git add crates/forge-core/src/object.rs crates/forge-core/src/contribution.rs crates/forge-core/tests/contribution.rs crates/forge-api/src/fsck.rs crates
git commit -m 'fix: complete bounded Contribution commit linkage'
git push origin HEAD:objects/contribution-108-v3
