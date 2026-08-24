#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path

api = Path('crates/forge-api/src/lib.rs')
s = api.read_text()
old = '''    pub fn import_dir(&self, cap: &Cap, dir: &Path, r#ref: &str) -> Result<ObjectId> {
        self.check(cap, Op::Write, Some(r#ref))?;
        let previous = self.store.meta.get_ref(r#ref)?;
        let previous_commit = match previous.as_ref() {
            Some(row) => Some(self.store.get_commit(row.oid)?),
            None => None,
        };
        let tree = import_walk(&self.store, dir, true)?;
        let parents = previous
            .as_ref()
            .map(|row| vec![row.oid])
            .unwrap_or_default();
        let commit = Commit {
            tree,
            parents,
            agent: cap.agent_id().into(),
            msg: format!("import {}", dir.display()),
            ts: now_ms(),
            landmark: false,
            contrib: None,
        };
        let cid = self.store.put_commit(&commit)?;
        let intro_oids = self
            .store
            .collect_intros(previous_commit.as_ref().map(|c| c.tree), tree)?;
        match previous {
            Some(row) => {
                self.store.meta.cas_ref_with_intros(
                    r#ref,
                    row.oid,
                    cid,
                    "commit",
                    cap.agent_id(),
                    cap.agent_id(),
                    false,
                    &intro_oids,
                )?;
            }
            None => {
                self.store.meta.insert_ref_with_intros(
                    r#ref,
                    cid,
                    "commit",
                    false,
                    false,
                    cap.agent_id(),
                    "import",
                    &intro_oids,
                )?;
            }
        }
        Ok(cid)
    }
'''
new = '''    pub fn import_dir(&self, cap: &Cap, dir: &Path, r#ref: &str) -> Result<CasResult> {
        self.check(cap, Op::Write, Some(r#ref))?;
        let previous = self.store.meta.get_ref(r#ref)?;
        let previous_commit = match previous.as_ref() {
            Some(row) => Some(self.store.get_commit(row.oid)?),
            None => None,
        };
        import_snapshot_barrier()?;
        let tree = import_walk(&self.store, dir, true)?;
        let parents = previous
            .as_ref()
            .map(|row| vec![row.oid])
            .unwrap_or_default();
        let commit = Commit {
            tree,
            parents,
            agent: cap.agent_id().into(),
            msg: format!("import {}", dir.display()),
            ts: now_ms(),
            landmark: false,
            contrib: None,
        };
        let cid = self.store.put_commit(&commit)?;
        let intro_oids = self
            .store
            .collect_intros(previous_commit.as_ref().map(|c| c.tree), tree)?;
        match previous {
            Some(row) => self.store.meta.cas_ref_with_intros(
                r#ref,
                row.oid,
                cid,
                "commit",
                cap.agent_id(),
                cap.agent_id(),
                false,
                &intro_oids,
            ),
            None => {
                self.store.meta.insert_ref_with_intros(
                    r#ref,
                    cid,
                    "commit",
                    false,
                    false,
                    cap.agent_id(),
                    "import",
                    &intro_oids,
                )?;
                Ok(CasResult::Updated {
                    name: r#ref.to_string(),
                    oid: cid,
                })
            }
        }
    }
'''
assert s.count(old) == 1, 'import_dir source drifted'
s = s.replace(old, new, 1)

marker = '''/// Debug-build-only process crash hook used by the cross-process init matrix.
'''
helper = '''/// Debug-build-only synchronization hook for the process-level import CAS race.\n/// Each participant has already snapshotted the target ref before entering this\n/// barrier, so two real CLI processes deterministically race the same expected OID.\nfn import_snapshot_barrier() -> Result<()> {\n    #[cfg(debug_assertions)]\n    if let Some(raw) = std::env::var_os("FORGEFS_TEST_IMPORT_SNAPSHOT_BARRIER") {\n        let dir = PathBuf::from(raw);\n        fs::create_dir_all(&dir)?;\n        fs::write(dir.join(std::process::id().to_string()), b"ready")?;\n        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);\n        loop {\n            let ready = fs::read_dir(&dir)?.filter_map(|entry| entry.ok()).count();\n            if ready >= 2 {\n                return Ok(());\n            }\n            if std::time::Instant::now() >= deadline {\n                return Err(Error::Busy(\n                    "timed out waiting for import snapshot test barrier".into(),\n                ));\n            }\n            std::thread::sleep(std::time::Duration::from_millis(5));\n        }\n    }\n    Ok(())\n}\n\n/// Debug-build-only process crash hook used by the cross-process init matrix.\n'''
assert s.count(marker) == 1, 'init crash hook marker drifted'
s = s.replace(marker, helper, 1)
api.write_text(s)

cli = Path('crates/forge-cli/src/main.rs')
s = cli.read_text()
old = '''        Cmd::Import { source, r#ref } => {
            if !source.is_dir() {
                return Err(Error::Invalid(format!(
                    "import source {} is not a directory",
                    source.display()
                )));
            }
            let id = f.import_dir(cap, &source, &r#ref)?;
            println!("imported {id} -> {ref_name}", ref_name = r#ref);
        }
'''
new = '''        Cmd::Import { source, r#ref } => {
            if !source.is_dir() {
                return Err(Error::Invalid(format!(
                    "import source {} is not a directory",
                    source.display()
                )));
            }
            match f.import_dir(cap, &source, &r#ref)? {
                CasResult::Updated { name, oid } => println!("imported {oid} -> {name}"),
                CasResult::Forked {
                    requested,
                    fork,
                    ours,
                    theirs,
                } => println!("forked {requested} -> {fork} ours={ours} theirs={theirs}"),
                CasResult::Noop { name, oid } => println!("noop {name} {oid}"),
            }
        }
'''
assert s.count(old) == 1, 'CLI import source drifted'
cli.write_text(s.replace(old, new, 1))

history = Path('crates/forge-api/tests/p0_authority_history.rs')
s = history.read_text()
s = s.replace('use forge_types::Error;\n', 'use forge_types::{CasResult, Error, ObjectId};\n', 1)
old = '''    fs::write(src.join("data.txt"), b"v1").unwrap();
    let first = f.import_dir(&root, &src, "imports/test").unwrap();
    let (_, c1) = f.peel_commit("imports/test").unwrap();
'''
new = '''    fn updated_oid(result: CasResult) -> ObjectId {
        match result {
            CasResult::Updated { oid, .. } => oid,
            other => panic!("sequential import unexpectedly did not update: {other:?}"),
        }
    }

    fs::write(src.join("data.txt"), b"v1").unwrap();
    let first = updated_oid(f.import_dir(&root, &src, "imports/test").unwrap());
    let (_, c1) = f.peel_commit("imports/test").unwrap();
'''
assert s.count(old) == 1, 'first history import drifted'
s = s.replace(old, new, 1)
old = '''    fs::write(src.join("data.txt"), b"v2").unwrap();
    let second = f.import_dir(&root, &src, "imports/test").unwrap();
    assert_ne!(first, second);
'''
new = '''    fs::write(src.join("data.txt"), b"v2").unwrap();
    let second = updated_oid(f.import_dir(&root, &src, "imports/test").unwrap());
    assert_ne!(first, second);
'''
assert s.count(old) == 1, 'second history import drifted'
history.write_text(s.replace(old, new, 1))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p forge-api --test p0_authority_history --locked
cargo test -p forge-api --test import_lossless --locked
cargo test -p forge-cli --test cli_import_race --locked

git add crates/forge-api/src/lib.rs crates/forge-cli/src/main.rs crates/forge-api/tests/p0_authority_history.rs crates/forge-cli/tests/cli_import_race.rs
git commit -m 'fix(import): return and report actual CAS outcome (#243)'
git push origin HEAD:fix/import-cas-result-243
