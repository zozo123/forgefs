#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main
git merge --no-edit origin/main

python3 - <<'PY'
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if text.count(old) != 1:
        raise SystemExit(f"{label}: expected exactly one marker, found {text.count(old)}")
    return text.replace(old, new, 1)

p = Path('crates/forge-store/src/meta.rs')
s = p.read_text()
# Only materialize the ref-kind change if this branch does not already contain it.
if 'name.starts_with("tags/") || name.starts_with("inbox/")' not in s:
    start = s.index('fn validate_ref_kind(name: &str, kind: &str) -> Result<()> {')
    end = s.index('\n}\n\nimpl Meta {', start) + 2
    new_fn = '''fn validate_ref_kind(name: &str, kind: &str) -> Result<()> {
    validate_ref_name(name)?;
    if let Some(rest) = name.strip_prefix("inbox/") {
        let mut parts = rest.split('/');
        let agent = parts.next().unwrap_or_default();
        let id = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || agent.is_empty()
            || sanitize_agent(agent) != agent
            || agent == "anon"
            || ulid::Ulid::from_string(id).is_err()
        {
            return Err(Error::Invalid(format!("invalid inbox ref {name:?}")));
        }
    }
    let expected = if name == "main" || name.starts_with("heads/") {
        Some("commit")
    } else if name.starts_with("forks/inbox/") {
        Some("snapshot")
    } else if name.starts_with("forks/") {
        Some("commit")
    } else if name.starts_with("conflicts/") {
        Some("conflict")
    } else if name.starts_with("tags/") || name.starts_with("inbox/") {
        Some("snapshot")
    } else {
        None
    };
    if let Some(expected) = expected {
        if kind != expected {
            return Err(Error::Invalid(format!(
                "ref {name} requires kind {expected}, got {kind}"
            )));
        }
    }
    Ok(())
}'''
    p.write_text(s[:start] + new_fn + s[end:])

p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
if 'pub fn inbox_push(' not in s:
    marker = '''    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let mut out = Vec::new();
        for r in self.store.meta.list_refs()? {
            if cap.allows(Op::Read, Some(&r.name), now_ms()).is_ok() {
                out.push(r);
            }
        }
        Ok(out)
    }'''
    addition = marker + '''

    /// Publish a sealed snapshot to a recipient-owned inbox ref.
    /// ForgeFS stores only the durable pointer; scheduling stays above the core.
    pub fn inbox_push(&self, cap: &Cap, to: &str, snapshot: &str) -> Result<CasResult> {
        let recipient = sanitize_agent(to);
        if recipient != to || recipient == "anon" {
            return Err(Error::Invalid(format!("invalid inbox recipient {to:?}")));
        }
        self.check_spec_read(cap, snapshot)?;
        let oid = self.resolve_spec_oid(snapshot)?;
        if self.store.object_type(oid)? != ObjectType::Snapshot {
            return Err(Error::Invalid("inbox payload must be a sealed snapshot".into()));
        }
        let name = format!("inbox/{recipient}/{}", ulid::Ulid::new());
        self.check(cap, Op::Write, Some(&name))?;
        self.store.meta.cas_ref(
            &name,
            ObjectId::ZERO,
            oid,
            "snapshot",
            cap.agent_id(),
            cap.agent_id(),
            false,
        )
    }

    /// List only the calling agent's concrete inbox refs that its cap can read.
    pub fn inbox_list(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let agent = cap.agent_id();
        if sanitize_agent(agent) != agent || agent == "anon" {
            return Err(Error::Invalid(format!("invalid inbox agent {agent:?}")));
        }
        let prefix = format!("inbox/{agent}/");
        let mut out = Vec::new();
        for row in self.store.meta.list_refs()? {
            if row.name.starts_with(&prefix)
                && cap.allows(Op::Read, Some(&row.name), now_ms()).is_ok()
            {
                out.push(row);
            }
        }
        Ok(out)
    }'''
    s = replace_once(s, marker, addition, 'inbox API insertion')
else:
    old = '''    pub fn inbox_list(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        verify(&self.hmac_key, cap)?;
        let agent = sanitize_agent(cap.agent_id());
        let prefix = format!("inbox/{agent}/");'''
    new = '''    pub fn inbox_list(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let agent = cap.agent_id();
        if sanitize_agent(agent) != agent || agent == "anon" {
            return Err(Error::Invalid(format!("invalid inbox agent {agent:?}")));
        }
        let prefix = format!("inbox/{agent}/");'''
    if old in s:
        s = s.replace(old, new, 1)
    elif new not in s:
        raise SystemExit('inbox_list marker drifted')
p.write_text(s)

p = Path('crates/forge-api/tests/inbox_refs.rs')
if not p.exists():
    p.write_text(r'''use forge_api::Forge;
use forge_types::CasResult;
use tempfile::tempdir;

#[test]
fn sealed_snapshot_can_be_published_to_recipient_inbox() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let snap = forge.seal(&root, "main", "v1.0").unwrap();
    let alice = forge
        .grant(
            &root,
            vec![
                "ops=read,write".into(),
                "agent=alice".into(),
                "ref=tags/v1.0,inbox/bob/*".into(),
            ],
        )
        .unwrap();
    let bob = forge
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "agent=bob".into(),
                "ref=inbox/bob/*".into(),
            ],
        )
        .unwrap();
    let result = forge.inbox_push(&alice, "bob", "tags/v1.0").unwrap();
    let CasResult::Updated { name, oid } = result else {
        panic!("new inbox ref must publish directly");
    };
    assert!(name.starts_with("inbox/bob/"));
    assert_eq!(oid, snap);
    let rows = forge.inbox_list(&bob).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, name);
    assert_eq!(rows[0].oid, snap);
    assert_eq!(rows[0].kind, "snapshot");
}

#[test]
fn inbox_write_requires_concrete_prefix_authority() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "v1.0").unwrap();
    let alice = forge
        .grant(
            &root,
            vec![
                "ops=read,write".into(),
                "agent=alice".into(),
                "ref=tags/v1.0,inbox/alice/*".into(),
            ],
        )
        .unwrap();
    assert!(forge.inbox_push(&alice, "bob", "tags/v1.0").is_err());
}

#[test]
fn invalid_inbox_recipient_fails_closed() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "v1.0").unwrap();
    assert!(forge.inbox_push(&root, "../bob", "tags/v1.0").is_err());
}

#[test]
fn inbox_list_requires_read_authority() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let write_only = forge
        .grant(
            &root,
            vec![
                "ops=write".into(),
                "agent=bob".into(),
                "ref=inbox/bob/*".into(),
            ],
        )
        .unwrap();
    assert!(forge.inbox_list(&write_only).is_err());
}
''')
else:
    t = p.read_text()
    if 'inbox_list_requires_read_authority' not in t:
        t += r'''

#[test]
fn inbox_list_requires_read_authority() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let write_only = forge
        .grant(
            &root,
            vec![
                "ops=write".into(),
                "agent=bob".into(),
                "ref=inbox/bob/*".into(),
            ],
        )
        .unwrap();
    assert!(forge.inbox_list(&write_only).is_err());
}
'''
        p.write_text(t)
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked

git rm -f .github/workflows/apply-inbox-v3.yml 2>/dev/null || true
git add crates/forge-api/src/lib.rs crates/forge-store/src/meta.rs crates/forge-api/tests/inbox_refs.rs
git commit -m 'agents: harden capability-scoped snapshot inbox refs (#111)'
git push origin HEAD:agents/inbox-111-v3
