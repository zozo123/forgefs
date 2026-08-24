#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path

p = Path('crates/forge-api/src/lib.rs')
s = p.read_text()
old = '''    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        if root.exists() {
            if root.join("VERSION").exists() {
                validate_repo_version(&root)?;
                return Err(Error::Invalid(format!(
                    "already a forge: {}",
                    root.display()
                )));
            }
            return Err(Error::Invalid(format!(
                "{} already exists without a ForgeFS VERSION; refusing to overwrite",
                root.display()
            )));
        }

        // Build completely under a sibling name. Publication is one atomic,
        // no-replace rename; VERSION remains the validity marker written last.
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_dir_all_durable(parent)?;
        let base = root
'''
new = '''    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_dir_all_durable(parent)?;

        // Serialize initializers on the repository's parent directory itself:
        // no persistent lock artifact is needed, and a crash releases the
        // kernel lock. Once held, every matching sibling staging directory is
        // from a previous failed initializer and can be reclaimed without
        // racing another current ForgeFS initializer.
        let _init_parent_lock = acquire_init_parent_lock(parent)?;
        cleanup_init_staging_siblings(&root)?;

        if root.exists() {
            if root.join("VERSION").exists() {
                validate_repo_version(&root)?;
                return Err(Error::Invalid(format!(
                    "already a forge: {}",
                    root.display()
                )));
            }
            return Err(Error::Invalid(format!(
                "{} already exists without a ForgeFS VERSION; refusing to overwrite",
                root.display()
            )));
        }

        // Build completely under a sibling name. Publication is one atomic,
        // no-replace rename; VERSION remains the validity marker written last.
        let base = root
'''
assert s.count(old) == 1, 'init prologue drifted'
s = s.replace(old, new, 1)

marker = '''fn validate_repo_version(root: &Path) -> Result<()> {
'''
helper = r'''fn init_staging_siblings(root: &Path) -> Result<Vec<PathBuf>> {
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

fn cleanup_init_staging_siblings(root: &Path) -> Result<()> {
    let paths = init_staging_siblings(root)?;
    if paths.is_empty() {
        return Ok(());
    }
    for path in &paths {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(Error::Invalid(format!(
                "reserved ForgeFS init staging path is not a directory: {}",
                path.display()
            )));
        }
        fs::remove_dir_all(path)?;
    }
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_dir(parent)?;
    Ok(())
}

fn acquire_init_parent_lock(parent: &Path) -> Result<File> {
    let file = File::open(parent)?;
    if !file.metadata()?.is_dir() {
        return Err(Error::Invalid(format!(
            "ForgeFS init parent is not a directory: {}",
            parent.display()
        )));
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err(Error::Busy("another ForgeFS initializer owns this directory".into()))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn validate_repo_version(root: &Path) -> Result<()> {
'''
assert s.count(marker) == 1, 'version helper marker drifted'
s = s.replace(marker, helper, 1)
p.write_text(s)

p = Path('crates/forge-api/src/fsck.rs')
s = p.read_text()
old = '''        let mut report = FsckReport::new(full);
        let refs = self.store.meta.list_refs()?;
'''
new = '''        let mut report = FsckReport::new(full);
        for path in crate::init_staging_siblings(self.root())? {
            report.finding(
                "INIT_STAGING",
                format!("path:{}", path.display()),
                "orphaned repository-initialization staging path; rerun `forge init` to reclaim it",
            );
        }
        let refs = self.store.meta.list_refs()?;
'''
assert s.count(old) == 1, 'fsck report marker drifted'
p.write_text(s.replace(old, new, 1))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p forge-cli --test cli_init_reclaim --locked
cargo test -p forge-cli --test cli_init_race --locked
cargo test -p forge-cli --test e2e cli_init_crash_matrix_never_publishes_a_partial_repository --locked

git add crates/forge-api/src/lib.rs crates/forge-api/src/fsck.rs crates/forge-cli/tests/cli_init_reclaim.rs
git commit -m 'fix(init): reclaim crash-staging debris (#171)'
git push origin HEAD:fix/init-staging-reclaim-171
