#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main objects/contribution-108
git merge --no-edit origin/main

git show 13f509ff298328d7f3a3811d2454f3a791c4880a:.github/swarm-worker.sh > /tmp/contribution-worker.sh
python3 - <<'PY'
from pathlib import Path
p = Path('/tmp/contribution-worker.sh')
s = p.read_text()
s = s.replace(
    'git rm -f .github/workflows/autopatch-contribution-108.yml .github/worker-trigger-108 .github/swarm-worker.sh',
    'git rm -f .github/workflows/run-contribution-v3.yml',
)
s = s.replace(
    'git push origin HEAD:objects/contribution-108',
    'git push origin HEAD:objects/contribution-108-v3',
)
s = s.replace(
    'git add crates/forge-types/src/lib.rs crates/forge-core/src/lib.rs crates/forge-core/src/contribution.rs crates/forge-core/tests/contribution.rs fuzz/fuzz_targets/object_decode.rs testdata/canonical/contribution.hex testdata/canonical/contribution.oid',
    'git add crates/forge-types/src/lib.rs crates/forge-core/src/lib.rs crates/forge-core/src/contribution.rs crates/forge-core/tests/contribution.rs crates/forge-api/src/fsck.rs fuzz/fuzz_targets/object_decode.rs testdata/canonical/contribution.hex testdata/canonical/contribution.oid',
)
fsck_patch = r"""python3 - <<'PYFSCK'
from pathlib import Path
p = Path('crates/forge-api/src/fsck.rs')
s = p.read_text()
old_import = 'use forge_core::{decode_object_type, Blob, Commit, Conflict, Snapshot, Tree};'
new_import = 'use forge_core::{decode_object_type, Blob, Commit, Conflict, Contribution, Snapshot, Tree};'
if old_import not in s:
    raise SystemExit('fsck import marker drifted')
s = s.replace(old_import, new_import, 1)
marker = '            ObjectType::Conflict => Conflict::decode(&bytes).map(|conflict| {'
arm = '''            ObjectType::Contribution => Contribution::decode(&bytes).map(|contribution| {
                queue.push_back((
                    contribution.base,
                    Some(ObjectType::Commit),
                    format!("contribution:{id}:base"),
                ));
                queue.push_back((
                    contribution.tree,
                    Some(ObjectType::Tree),
                    format!("contribution:{id}:tree"),
                ));
                for parent in contribution.parents {
                    queue.push_back((
                        parent,
                        Some(ObjectType::Commit),
                        format!("contribution:{id}:parent"),
                    ));
                }
                for read in contribution.reads {
                    queue.push_back((
                        read.id,
                        None,
                        format!("contribution:{id}:read:{}", read.path),
                    ));
                }
            }),
'''
if marker not in s:
    raise SystemExit('fsck match marker drifted')
p.write_text(s.replace(marker, arm + marker, 1))
PYFSCK
"""
needle = 'cargo fmt --all\n'
if needle not in s:
    raise SystemExit('worker format marker drifted')
s = s.replace(needle, fsck_patch + needle, 1)
p.write_text(s)
PY
bash /tmp/contribution-worker.sh
