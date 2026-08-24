#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com

python3 - <<'PY'
from pathlib import Path

meta = Path('crates/forge-store/src/meta.rs')
s = meta.read_text()
old = '''    pub fn overlay_upsert(
        &self,
        ns_id: &str,
        mount: &str,
        path: &str,
        blob_oid: Option<ObjectId>,
        exec: bool,
    ) -> Result<()> {
        let conn = self.write.lock();
        let oid = blob_oid.map(|o| o.0.to_vec());
        conn.execute(
            "INSERT OR REPLACE INTO overlay (ns_id, mount, path, blob_oid, exec) VALUES (?1,?2,?3,?4,?5)",
            params![ns_id, mount, path, oid, exec as i64],
        )
        .map_err(map_sql)?;
        Ok(())
    }'''
new = '''    pub fn overlay_upsert(
        &self,
        ns_id: &str,
        mount: &str,
        path: &str,
        blob_oid: Option<ObjectId>,
        exec: bool,
    ) -> Result<()> {
        // I6/I10: an accepted overlay must describe one representable tree.
        // Make the prefix check and mutation one IMMEDIATE transaction so two
        // processes cannot concurrently stage an ancestor and descendant.
        // Exact-path replacement remains legal and keeps ordinary overwrite
        // semantics for a path already staged by this namespace.
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let conflict = tx
            .query_row(
                "SELECT path FROM overlay
                 WHERE ns_id=?1 AND mount=?2 AND path<>?3 AND (
                   (length(path) < length(?3)
                    AND substr(?3,1,length(path))=path
                    AND substr(?3,length(path)+1,1)='/')
                   OR
                   (length(path) > length(?3)
                    AND substr(path,1,length(?3))=?3
                    AND substr(path,length(?3)+1,1)='/')
                 )
                 ORDER BY path
                 LIMIT 1",
                params![ns_id, mount, path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sql)?;
        if let Some(existing) = conflict {
            return Err(Error::Invalid(format!(
                "overlay path conflict: {path} and {existing} cannot coexist"
            )));
        }
        let oid = blob_oid.map(|o| o.0.to_vec());
        tx.execute(
            "INSERT OR REPLACE INTO overlay (ns_id, mount, path, blob_oid, exec) VALUES (?1,?2,?3,?4,?5)",
            params![ns_id, mount, path, oid, exec as i64],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }'''
assert s.count(old) == 1, 'overlay_upsert source drifted'
meta.write_text(s.replace(old, new))

api = Path('crates/forge-api/src/lib.rs')
s = api.read_text()
old = '''        let rel = rel_of(&m.path, path)?;
        self.store
            .meta
            .overlay_upsert(ns, &m.path, &rel, None, false)?;
        Ok(())
    }

    pub fn checkin'''
new = '''        let rel = rel_of(&m.path, path)?;
        if rel.is_empty() {
            return Err(Error::Invalid("cannot delete mount root".into()));
        }
        self.store
            .meta
            .overlay_upsert(ns, &m.path, &rel, None, false)?;
        Ok(())
    }

    pub fn checkin'''
assert s.count(old) == 1, 'delete source drifted'
api.write_text(s.replace(old, new))
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo test -p forge-api --test overlay_prefix_conflict --locked

git add crates/forge-store/src/meta.rs crates/forge-api/src/lib.rs crates/forge-api/tests/overlay_prefix_conflict.rs
git commit -m 'fix(workspace): reject impossible overlay prefixes (#273)'
git push origin HEAD:fix/overlay-prefix-273
