#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path

p = Path('crates/forge-cap/src/lib.rs')
s = p.read_text()
old = '            "allow=read:main,heads/agents/*,forks/*,tags/*".into(),\n'
new = '            "allow=read:main,heads/agents/*,forks/*,tags/*,conflicts/*".into(),\n'
assert s.count(old) == 1, 'integrator read scope drifted'
p.write_text(s.replace(old, new, 1))

p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
old = '''    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let mut out = Vec::new();
        for r in self.store.meta.list_refs()? {
            if cap.allows(Op::Read, Some(&r.name), now_ms()).is_ok() {
                out.push(r);
            }
        }
        Ok(out)
    }
'''
new = '''    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        Ok(self.refs_with_suppressed(cap)?.0)
    }

    /// Enumerate refs visible to `cap` and return how many durable rows were
    /// hidden by ref authority. Names and contents remain undisclosed, but
    /// automation can distinguish an actually complete view from a filtered one.
    pub fn refs_with_suppressed(&self, cap: &Cap) -> Result<(Vec<RefRow>, usize)> {
        self.check(cap, Op::Read, None)?;
        let now = now_ms();
        let mut out = Vec::new();
        let mut suppressed = 0usize;
        for r in self.store.meta.list_refs()? {
            if cap.allows(Op::Read, Some(&r.name), now).is_ok() {
                out.push(r);
            } else {
                suppressed += 1;
            }
        }
        Ok((out, suppressed))
    }
'''
assert s.count(old) == 1, 'refs implementation drifted'
p.write_text(s.replace(old, new, 1))

p = Path('crates/forge-cli/src/main.rs')
s = p.read_text()
old = '''        Cmd::Refs => {
            for r in f.refs(cap)? {
                let flags = format!(
                    "{}{}",
                    if r.protected { "P" } else { "-" },
                    if r.sealed { "S" } else { "-" }
                );
                println!("{flags} {:<32} {} {}", r.kind, r.name, r.oid);
            }
        }
'''
new = '''        Cmd::Refs => {
            let (refs, suppressed) = f.refs_with_suppressed(cap)?;
            for r in refs {
                let flags = format!(
                    "{}{}",
                    if r.protected { "P" } else { "-" },
                    if r.sealed { "S" } else { "-" }
                );
                println!("{flags} {:<32} {} {}", r.kind, r.name, r.oid);
            }
            if suppressed > 0 {
                eprintln!("{suppressed} ref(s) suppressed by authority");
            }
        }
'''
assert s.count(old) == 1, 'CLI refs implementation drifted'
p.write_text(s.replace(old, new, 1))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p forge-api --test integrator_conflict_visibility --locked
cargo test -p forge-cli --test refs_suppression --locked

git add crates/forge-cap/src/lib.rs crates/forge-api/src/lib.rs crates/forge-cli/src/main.rs crates/forge-api/tests/integrator_conflict_visibility.rs crates/forge-cli/tests/refs_suppression.rs
git commit -m 'fix(auth): make conflict visibility loud (#235)'
git push origin HEAD:fix/conflict-visibility-235
