#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main
git merge --no-edit origin/main

python3 - <<'PY'
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one marker, found {count}")
    return text.replace(old, new, 1)

# Store: Contribution is a first-class durable object and can participate in a
# checkin publish batch before metadata CAS.
p = Path("crates/forge-store/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    "use forge_core::object::{decode_object_type, Blob, Commit, Conflict, Snapshot};\n",
    "use forge_core::object::{decode_object_type, Blob, Commit, Conflict, Snapshot};\nuse forge_core::Contribution;\n",
    "store contribution import",
)
s = replace_once(
    s,
    "    pub fn put_commit(&self, commit: &Commit) -> Result<ObjectId> {\n        self.objects.lock().put(&commit.encode())\n    }\n",
    "    pub fn put_commit(&self, commit: &Commit) -> Result<ObjectId> {\n        self.objects.lock().put(&commit.encode())\n    }\n\n    pub fn put_contribution(&self, contribution: &Contribution) -> Result<ObjectId> {\n        let bytes = contribution.encode()?;\n        self.objects.lock().put(&bytes)\n    }\n",
    "batch put contribution",
)
s = replace_once(
    s,
    "    pub fn get_commit(&self, id: ObjectId) -> Result<Commit> {\n        Commit::decode(&self.get_raw(id)?)\n    }\n",
    "    pub fn get_commit(&self, id: ObjectId) -> Result<Commit> {\n        Commit::decode(&self.get_raw(id)?)\n    }\n\n    pub fn put_contribution(&self, contribution: &Contribution) -> Result<ObjectId> {\n        self.put_raw(&contribution.encode()?)\n    }\n\n    pub fn get_contribution(&self, id: ObjectId) -> Result<Contribution> {\n        Contribution::decode(&self.get_raw(id)?)\n    }\n",
    "store contribution accessors",
)
p.write_text(s)

# API: derive one canonical receipt from the exact session state that checkin
# validates and commits. Use one timestamp for receipt+commit.
p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    "use forge_core::{hash_bytes, now_ms, Blob, Commit, Conflict, Snapshot, Tree};\n",
    "use forge_core::{\n    hash_bytes, now_ms, Blob, Commit, Conflict, Contribution, ContributionRead, Snapshot, Tree,\n};\n",
    "api contribution imports",
)
s = replace_once(
    s,
    "        let ov_rows = self.store.meta.overlay_list(ns, &m.path)?;\n        let ov = overlay_map(&ov_rows);\n        self.check_observations(ns, &m.path, &ov, pin, &mounts)?;\n",
    "        let ov_rows = self.store.meta.overlay_list(ns, &m.path)?;\n        let observations = self.store.meta.observations(ns)?;\n        let ov = overlay_map(&ov_rows);\n        self.check_observations(ns, &m.path, &ov, pin, &mounts)?;\n",
    "checkin capture observations",
)
old_commit = '''        let commit = Commit {
            tree: new_tree,
            parents: vec![pin],
            agent: cap.agent_id().into(),
            msg: msg.into(),
            ts: now_ms(),
            landmark: false,
            contrib: None,
        };
        let cid = batch.put_commit(&commit)?;
'''
new_commit = '''        let mut reads = observations
            .into_iter()
            .map(|obs| ContributionRead {
                path: contribution_path(&obs.mount, &obs.path),
                id: obs.oid,
            })
            .collect::<Vec<_>>();
        reads.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        for pair in reads.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(Error::Invalid(format!(
                    "ambiguous contribution read path {}",
                    pair[0].path
                )));
            }
        }
        let mut writes = ov_rows
            .iter()
            .map(|row| contribution_path(&m.path, &row.path))
            .collect::<Vec<_>>();
        writes.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        writes.dedup();

        let ts = now_ms();
        let contribution = Contribution {
            base: pin,
            tree: new_tree,
            parents: vec![pin],
            reads,
            writes,
            agent: cap.agent_id().into(),
            ts,
        };
        let contribution_oid = batch.put_contribution(&contribution)?;
        let commit = Commit {
            tree: new_tree,
            parents: vec![pin],
            agent: cap.agent_id().into(),
            msg: msg.into(),
            ts,
            landmark: false,
            contrib: Some(contribution_oid),
        };
        let cid = batch.put_commit(&commit)?;
'''
s = replace_once(s, old_commit, new_commit, "checkin receipt commit")

# `forge show oid:<receipt>` is the smallest inspection surface needed by agents
# and by #109 acceptance; keep it deterministic and text-only.
show_marker = '''        if ty == ObjectType::Conflict {
            let conflict = self.store.get_conflict(oid)?;
'''
show_replacement = '''        if ty == ObjectType::Contribution {
            let contribution = self.store.get_contribution(oid)?;
            let mut out = String::new();
            out.push_str(&format!("contribution {oid}\\n"));
            out.push_str(&format!("agent {}\\n", contribution.agent));
            out.push_str(&format!("base {}\\n", contribution.base));
            out.push_str(&format!("tree {}\\n", contribution.tree));
            out.push_str(&format!(
                "parents {}\\n",
                contribution
                    .parents
                    .iter()
                    .map(ObjectId::hex)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            for read in contribution.reads {
                out.push_str(&format!("read {} {}\\n", read.id, read.path));
            }
            for path in contribution.writes {
                out.push_str(&format!("write {path}\\n"));
            }
            return Ok(out.trim_end().to_string());
        }
        if ty == ObjectType::Conflict {
            let conflict = self.store.get_conflict(oid)?;
'''
s = replace_once(s, show_marker, show_replacement, "show contribution")

# Canonicalize a session-relative path to the visible absolute namespace path so
# reads from multiple mounts cannot alias in one receipt.
helper_marker = '''fn blob_at(store: &Store, tree: ObjectId, rel: &str) -> Result<Option<ObjectId>> {
'''
helper = '''fn contribution_path(mount: &str, rel: &str) -> String {
    if rel.is_empty() {
        return mount.to_string();
    }
    if mount == "/" {
        format!("/{rel}")
    } else {
        format!("{}/{}", mount.trim_end_matches('/'), rel)
    }
}

fn blob_at(store: &Store, tree: ObjectId, rel: &str) -> Result<Option<ObjectId>> {
'''
s = replace_once(s, helper_marker, helper, "contribution path helper")
p.write_text(s)

# API regression: first checkin records writes; second checkin records both a
# read and a write and binds receipt -> exact commit tree/base/agent.
t = Path("crates/forge-api/tests/checkin_contribution.rs")
t.write_text(r'''use forge_api::Forge;
use forge_types::{CasResult, ObjectId};
use tempfile::tempdir;

fn updated_oid(result: CasResult) -> ObjectId {
    match result {
        CasResult::Updated { oid, .. } | CasResult::Forked { ours: oid, .. } => oid,
        CasResult::Noop { .. } => panic!("expected a material checkin"),
    }
}

#[test]
fn checkin_persists_showable_contribution_from_reads_and_writes() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    forge.write(&root, &ns, "/a.txt", b"a", false).unwrap();
    let first_oid = updated_oid(forge.checkin(&root, &ns, "/", "first").unwrap());
    let (_, first) = forge.peel_commit(&format!("oid:{}", first_oid)).unwrap();
    let first_contrib = first.contrib.expect("material checkin must have a receipt");
    let first_show = forge
        .show(&root, &format!("oid:{first_contrib}"))
        .unwrap();
    assert!(first_show.contains("write /a.txt"));
    assert!(first_show.contains(&format!("tree {}", first.tree)));

    assert_eq!(forge.read(&root, &ns, "/a.txt").unwrap(), b"a");
    forge.write(&root, &ns, "/b.txt", b"b", false).unwrap();
    let second_oid = updated_oid(forge.checkin(&root, &ns, "/", "second").unwrap());
    let (_, second) = forge.peel_commit(&format!("oid:{}", second_oid)).unwrap();
    let second_contrib = second.contrib.expect("material checkin must have a receipt");
    let shown = forge
        .show(&root, &format!("oid:{second_contrib}"))
        .unwrap();

    assert!(shown.contains("read "));
    assert!(shown.contains(" /a.txt"));
    assert!(shown.contains("write /b.txt"));
    assert!(shown.contains(&format!("base {}", first_oid)));
    assert!(shown.contains(&format!("tree {}", second.tree)));
    assert!(shown.contains("agent root"));
}

#[test]
fn noop_checkin_does_not_invent_a_receipt() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    assert!(matches!(
        forge.checkin(&root, &ns, "/", "noop").unwrap(),
        CasResult::Noop { .. }
    ));
}
''')
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked

git rm -f .github/trigger-checkin-contribution-109 2>/dev/null || true
git add crates/forge-store/src/lib.rs crates/forge-api/src/lib.rs crates/forge-api/tests/checkin_contribution.rs
git commit -m 'agents: persist Contribution receipts on checkin (#109)'
git push origin HEAD:agents/checkin-contribution-109
