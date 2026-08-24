#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path
p = Path('crates/forge-merge/src/lib.rs')
s = p.read_text()
old = '''            (Some(x), None, g) => {
                if g.map(|g| g.id) == Some(x.id) {
                    // deleted on theirs, unchanged ours → take delete
'''
new = '''            (Some(x), None, g) => {
                if g.is_some_and(|g| same_entry(g, x)) {
                    // deleted on theirs, unchanged ours → take delete
'''
assert s.count(old) == 1, 'ours deletion-side branch drifted'
s = s.replace(old, new, 1)
old = '''            (None, Some(x), g) => {
                if g.map(|g| g.id) == Some(x.id) {
                    // deleted on ours
'''
new = '''            (None, Some(x), g) => {
                if g.is_some_and(|g| same_entry(g, x)) {
                    // deleted on ours
'''
assert s.count(old) == 1, 'theirs deletion-side branch drifted'
p.write_text(s.replace(old, new, 1))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p forge-merge --locked

git add crates/forge-merge/src/lib.rs crates/forge-merge/tests/delete_metadata.rs
git commit -m 'fix(merge): preserve metadata against deletion (#293)'
git push origin HEAD:fix/delete-metadata-293
