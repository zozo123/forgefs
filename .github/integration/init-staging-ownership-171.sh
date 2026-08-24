#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path

p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
old = '''fn init_staging_siblings(root: &Path) -> Result<Vec<PathBuf>> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".forge");
    let prefix = format!("{base}.init-");
    let mut paths = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
'''
new = '''fn init_staging_siblings(root: &Path) -> Result<Vec<PathBuf>> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".forge");
    let prefix = format!("{base}.init-");
    let mut paths = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((pid, ulid)) = suffix.split_once('-') else {
            continue;
        };
        if pid.is_empty()
            || !pid.bytes().all(|byte| byte.is_ascii_digit())
            || ulid.parse::<ulid::Ulid>().is_err()
        {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}
'''
assert s.count(old) == 1, 'init staging scan drifted'
p.write_text(s.replace(old, new, 1))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p forge-api --test bootstrap_contract --locked
cargo test -p forge-cli --test cli_init_reclaim --locked
cargo test -p forge-cli --test cli_init_race --locked

git add crates/forge-api/src/lib.rs crates/forge-cli/tests/cli_init_reclaim.rs
git commit -m 'fix(init): reclaim only owned staging names (#171)'
git push origin HEAD:fix/init-staging-reclaim-171
