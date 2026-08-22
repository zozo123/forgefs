#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main
set +e
git merge --no-edit origin/main
status=$?
set -e
if [ "$status" -ne 0 ]; then
  unresolved="$(git diff --name-only --diff-filter=U | sort)"
  while IFS= read -r f; do
    case "$f" in
      crates/forge-api/src/lib.rs|crates/forge-store/src/meta.rs) git checkout --theirs "$f"; git add "$f" ;;
      "") ;;
      *) echo "unexpected conflict: $f"; exit 1 ;;
    esac
  done <<EOF
$unresolved
EOF
fi

python3 - <<'PY'
from pathlib import Path

p = Path('crates/forge-store/src/meta.rs')
s = p.read_text()
old = '''    let expected = if name == "main" || name.starts_with("heads/") || name.starts_with("forks/") {\n        Some("commit")\n    } else if name.starts_with("conflicts/") {\n        Some("conflict")\n    } else if name.starts_with("tags/") {\n        Some("snapshot")\n    } else {\n        None\n    };'''
new = '''    if let Some(rest) = name.strip_prefix("inbox/") {\n        let mut parts = rest.split('/');\n        let agent = parts.next().unwrap_or_default();\n        let id = parts.next().unwrap_or_default();\n        if parts.next().is_some()\n            || agent.is_empty()\n            || sanitize_agent(agent) != agent\n            || ulid::Ulid::from_string(id).is_err()\n        {\n            return Err(Error::Invalid(format!("invalid inbox ref {name:?}")));\n        }\n    }\n\n    let expected = if name == "main" || name.starts_with("heads/") {\n        Some("commit")\n    } else if name.starts_with("forks/inbox/") {\n        Some("snapshot")\n    } else if name.starts_with("forks/") {\n        Some("commit")\n    } else if name.starts_with("conflicts/") {\n        Some("conflict")\n    } else if name.starts_with("tags/") || name.starts_with("inbox/") {\n        Some("snapshot")\n    } else {\n        None\n    };'''
if old not in s:
    # Normalize an older inbox implementation if it survived the merge.
    start = s.index('    let expected = if name == "main"')
    end = s.index('    if let Some(expected) = expected {', start)
    s = s[:start] + new + '\n' + s[end:]
else:
    s = s.replace(old, new)
p.write_text(s)

p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
marker = '''    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {\n        self.check(cap, Op::Read, None)?;\n        let mut out = Vec::new();\n        for r in self.store.meta.list_refs()? {\n            if cap.allows(Op::Read, Some(&r.name), now_ms()).is_ok() {\n                out.push(r);\n            }\n        }\n        Ok(out)\n    }'''
addition = marker + '''\n\n    /// Publish an immutable sealed snapshot to a recipient-owned inbox ref.\n    /// ForgeFS stores only the durable pointer; scheduling and messaging stay above the core.\n    pub fn inbox_push(&self, cap: &Cap, to: &str, snapshot: &str) -> Result<CasResult> {\n        let recipient = sanitize_agent(to);\n        if recipient != to || recipient == "anon" {\n            return Err(Error::Invalid(format!("invalid inbox recipient {to:?}")));\n        }\n        self.check_spec_read(cap, snapshot)?;\n        let oid = self.resolve_spec_oid(snapshot)?;\n        if self.store.object_type(oid)? != ObjectType::Snapshot {\n            return Err(Error::Invalid("inbox payload must be a sealed snapshot".into()));\n        }\n        let name = format!("inbox/{recipient}/{}", ulid::Ulid::new());\n        self.check(cap, Op::Write, Some(&name))?;\n        self.store.meta.cas_ref(\n            &name,\n            ObjectId([0; 32]),\n            oid,\n            "snapshot",\n            cap.agent_id(),\n            cap.agent_id(),\n            false,\n        )\n    }\n\n    /// List only the calling agent's concrete inbox refs that its capability can read.\n    pub fn inbox_list(&self, cap: &Cap) -> Result<Vec<RefRow>> {\n        verify(&self.hmac_key, cap)?;\n        let agent = sanitize_agent(cap.agent_id());\n        let prefix = format!("inbox/{agent}/");\n        let mut out = Vec::new();\n        for row in self.store.meta.list_refs()? {\n            if row.name.starts_with(&prefix)\n                && cap.allows(Op::Read, Some(&row.name), now_ms()).is_ok()\n            {\n                out.push(row);\n            }\n        }\n        Ok(out)\n    }'''
if 'pub fn inbox_push(&self' in s:
    start = s.index('    /// Publish an immutable sealed snapshot')
    end = s.index('\n    pub fn log(', start)
    s = s[:start] + addition[len(marker):] + s[end:]
else:
    assert s.count(marker) == 1
    s = s.replace(marker, addition)
p.write_text(s)
PY

cargo fmt --all
cargo test --locked -p forge-api --test inbox_refs
cargo check --locked --workspace --all-targets
rm -f .github/workflows/rebase-inbox-168.yml .github/workflows/autopatch-inbox-111.yml .github/worker-trigger-168
git rm -f .github/swarm-worker.sh || true
git add -A
git commit -m 'merge main: compose capability-scoped inbox refs (#168)'
git push origin HEAD:agents/inbox-111
