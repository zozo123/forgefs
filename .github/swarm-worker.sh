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
      crates/forge-api/src/lib.rs) git checkout --theirs "$f"; git add "$f" ;;
      "") ;;
      *) echo "unexpected conflict: $f"; exit 1 ;;
    esac
  done <<EOF
$unresolved
EOF
fi

python3 - <<'PY'
from pathlib import Path
p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
old = '''    pub fn show(&self, cap: &Cap, spec: &str) -> Result<String> {\n        self.check_spec_read(cap, spec)?;\n        let oid = self.resolve_spec_oid(spec)?;\n        let bytes = self.store.get_raw(oid)?;\n        Ok(format!(\n            "{} {} bytes",\n            self.store.object_type(oid)?.as_str(),\n            bytes.len()\n        ))\n    }'''
new = '''    pub fn show(&self, cap: &Cap, spec: &str) -> Result<String> {\n        self.check_spec_read(cap, spec)?;\n        let oid = self.resolve_spec_oid(spec)?;\n        let ty = self.store.object_type(oid)?;\n        if ty == ObjectType::Conflict {\n            let conflict = self.store.get_conflict(oid)?;\n            let fmt_oid = |id: Option<ObjectId>| {\n                id.map(|v| v.hex()).unwrap_or_else(|| "-".into())\n            };\n            let mut out = String::new();\n            out.push_str(&format!("conflict {oid}\\n"));\n            out.push_str(&format!(\n                "bases {}\\n",\n                if conflict.bases.is_empty() {\n                    "-".into()\n                } else {\n                    conflict\n                        .bases\n                        .iter()\n                        .map(ObjectId::hex)\n                        .collect::<Vec<_>>()\n                        .join(",")\n                }\n            ));\n            out.push_str(&format!("ours {}\\n", conflict.ours));\n            out.push_str(&format!("theirs {}\\n", conflict.theirs));\n            for path in conflict.paths {\n                out.push_str(&format!(\n                    "path {} a={} b={} base={}\\n",\n                    path.path,\n                    fmt_oid(path.a),\n                    fmt_oid(path.b),\n                    fmt_oid(path.base)\n                ));\n            }\n            if !conflict.causal.is_empty() {\n                out.push_str(&format!(\n                    "causal {}\\n",\n                    conflict\n                        .causal\n                        .iter()\n                        .map(ObjectId::hex)\n                        .collect::<Vec<_>>()\n                        .join(",")\n                ));\n            }\n            return Ok(out.trim_end().to_string());\n        }\n        let bytes = self.store.get_raw(oid)?;\n        Ok(format!("{} {} bytes", ty.as_str(), bytes.len()))\n    }'''
if old not in s:
    # If the old branch implementation survived the merge, normalize by replacing it.
    start = s.index('    pub fn show(&self, cap: &Cap, spec: &str) -> Result<String> {')
    end = s.index('\n    pub fn fsck(', start)
    s = s[:start] + new + s[end:]
else:
    s = s.replace(old, new)
p.write_text(s)
PY

cargo fmt --all
cargo test --locked -p forge-api --test show_conflict
cargo check --locked --workspace --all-targets
rm -f .github/workflows/rebase-conflict-show-138.yml .github/workflows/autofix-conflict-138.yml .github/worker-trigger-138
git rm -f .github/swarm-worker.sh || true
git add -A
git commit -m 'merge main: compose actionable conflict show (#138)'
git push origin HEAD:cli/show-conflict-106
