from pathlib import Path

store_p = Path("crates/forge-store/src/lib.rs")
store = store_p.read_text()
old = '''    pub fn record_intros(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {
        let mut oids = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut oids)?;
        self.meta.intro_insert_many(&oids, commit, agent)
    }
'''
new = '''    pub fn collect_intros(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<Vec<ObjectId>> {
        let mut oids = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut oids)?;
        Ok(oids)
    }

    pub fn record_intros(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {
        let oids = self.collect_intros(old, new)?;
        self.meta.intro_insert_many(&oids, commit, agent)
    }
'''
if old not in store:
    raise SystemExit("record_intros anchor changed")
store = store.replace(old, new, 1)
store_p.write_text(store)

meta_p = Path("crates/forge-store/src/meta.rs")
meta = meta_p.read_text()
if "fn insert_intros_tx(" in meta:
    raise SystemExit("provenance atomicity already applied")

# Transaction helper before insert_ref.
pos = meta.index("    #[allow(clippy::too_many_arguments)]\n    pub fn insert_ref(")
helper = '''    fn insert_intros_tx(
        tx: &rusqlite::Transaction<'_>,
        oids: &[ObjectId],
        commit: ObjectId,
        agent_id: &str,
        ts: i64,
    ) -> Result<()> {
        for oid in oids {
            tx.execute(
                "INSERT OR IGNORE INTO object_intro (oid, commit_oid, agent_id, ts_ms) VALUES (?1,?2,?3,?4)",
                params![oid.as_bytes().as_slice(), commit.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
        }
        Ok(())
    }

'''
meta = meta[:pos] + helper + meta[pos:]

# Clone insert_ref into an intros-aware variant while preserving current API.
start = meta.index("    #[allow(clippy::too_many_arguments)]\n    pub fn insert_ref(")
end = meta.index("\n    pub fn update_mount_spec(", start)
orig = meta[start:end]
variant = orig.replace("pub fn insert_ref(", "pub fn insert_ref_with_intros(", 1)
needle = "        reason: &str,\n    ) -> Result<()> {"
if needle not in variant:
    raise SystemExit("insert_ref signature changed")
variant = variant.replace(
    needle,
    "        reason: &str,\n        intro_oids: &[ObjectId],\n    ) -> Result<()> {",
    1,
)
needle = "        tx.commit().map_err(map_sql)?;"
if variant.count(needle) != 1:
    raise SystemExit("insert_ref commit shape changed")
variant = variant.replace(
    needle,
    "        Self::insert_intros_tx(&tx, intro_oids, oid, agent_id, ts)?;\n" + needle,
    1,
)
meta = meta[:end] + "\n\n" + variant + meta[end:]

# Clone generic CAS into an intros-aware variant, adding intros before every successful commit.
start = meta.index("    #[allow(clippy::too_many_arguments)]\n    pub fn cas_ref(")
end = meta.index("\n    #[allow(clippy::too_many_arguments)]\n    pub fn cas_ref_session(", start)
orig = meta[start:end]
variant = orig.replace("pub fn cas_ref(", "pub fn cas_ref_with_intros(", 1)
needle = "        allow_protected: bool,\n    ) -> Result<CasResult> {"
if needle not in variant:
    raise SystemExit("cas_ref signature changed")
variant = variant.replace(
    needle,
    "        allow_protected: bool,\n        intro_oids: &[ObjectId],\n    ) -> Result<CasResult> {",
    1,
)
commit_line = "            tx.commit().map_err(map_sql)?;"
if variant.count(commit_line) != 2:
    raise SystemExit(f"cas_ref expected 2 indented commits, got {variant.count(commit_line)}")
variant = variant.replace(
    commit_line,
    "            Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;\n" + commit_line,
)
last_commit = "        tx.commit().map_err(map_sql)?;"
# The indented occurrences above also contain this substring; target the final exact tail occurrence.
idx = variant.rfind(last_commit)
if idx < 0:
    raise SystemExit("cas_ref final commit missing")
variant = variant[:idx] + "        Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;\n" + variant[idx:]
meta = meta[:end] + "\n\n" + variant + meta[end:]

# Session CAS: accept intro set and commit it with ref + session cleanup.
start = meta.index("    pub fn cas_ref_session(")
end = meta.index("\n    pub fn complete_noop_session(", start)
block = meta[start:end]
needle = "        mount_path: &str,\n    ) -> Result<CasResult> {"
if needle not in block:
    raise SystemExit("cas_ref_session signature changed")
block = block.replace(
    needle,
    "        mount_path: &str,\n        intro_oids: &[ObjectId],\n    ) -> Result<CasResult> {",
    1,
)
needle = "        tx.commit().map_err(map_sql)?;\n        Ok(result)"
if needle not in block:
    raise SystemExit("cas_ref_session commit changed")
block = block.replace(
    needle,
    "        Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;\n        tx.commit().map_err(map_sql)?;\n        Ok(result)",
    1,
)
meta = meta[:start] + block + meta[end:]

# Reuse helper for standalone batches.
start = meta.index("    pub fn intro_insert_many(")
end = meta.index("\n    pub fn intro_get(", start)
block = meta[start:end]
body_start = block.index("    ) -> Result<()> {") + len("    ) -> Result<()> {")
new_body = '''
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let ts = now_ms() as i64;
        Self::insert_intros_tx(&tx, oids, commit, agent_id, ts)?;
        tx.commit().map_err(map_sql)?;
        Ok(())
    }'''
block = block[:body_start] + new_body
meta = meta[:start] + block + meta[end:]
meta_p.write_text(meta)

api_p = Path("crates/forge-api/src/lib.rs")
api = api_p.read_text()

# Init: publish provenance with the initial main ref.
old = '''        let cid = store.put_commit(&commit)?;
        store.record_intros(None, empty, cid, "init")?;
        store
            .meta
            .insert_ref("main", cid, "commit", true, false, "init", "init")?;
'''
new = '''        let cid = store.put_commit(&commit)?;
        let intro_oids = store.collect_intros(None, empty)?;
        store.meta.insert_ref_with_intros(
            "main",
            cid,
            "commit",
            true,
            false,
            "init",
            "init",
            &intro_oids,
        )?;
'''
if old not in api:
    raise SystemExit("init provenance anchor changed")
api = api.replace(old, new, 1)

# Checkin: collect first, publish intros inside session transaction.
old = '''        let cid = self.store.put_commit(&commit)?;
        self.store
            .record_intros(Some(base_commit.tree), new_tree, cid, cap.agent_id())?;
        let result = self.store.meta.cas_ref_session(
            &ref_name,
            pin,
            cid,
            cap.agent_id(),
            cap.agent_id(),
            ns,
            &m.path,
        )?;
'''
new = '''        let cid = self.store.put_commit(&commit)?;
        let intro_oids = self.store.collect_intros(Some(base_commit.tree), new_tree)?;
        let result = self.store.meta.cas_ref_session(
            &ref_name,
            pin,
            cid,
            cap.agent_id(),
            cap.agent_id(),
            ns,
            &m.path,
            &intro_oids,
        )?;
'''
if old not in api:
    raise SystemExit("checkin provenance anchor changed")
api = api.replace(old, new, 1)

# Merge: bind intros to the CAS/fork publication.
old = '''        let cid = self.store.put_commit(&commit)?;
        self.store
            .record_intros(Some(ours_c.tree), tree, cid, cap.agent_id())?;
        self.store.meta.cas_ref(
            into,
            into_row.oid,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
            into_row.protected,
        )
'''
new = '''        let cid = self.store.put_commit(&commit)?;
        let intro_oids = self.store.collect_intros(Some(ours_c.tree), tree)?;
        self.store.meta.cas_ref_with_intros(
            into,
            into_row.oid,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
            into_row.protected,
            &intro_oids,
        )
'''
if old not in api:
    raise SystemExit("merge provenance anchor changed")
api = api.replace(old, new, 1)

# Import: bind intros to either first ref creation or CAS/fork.
old = '''        let cid = self.store.put_commit(&commit)?;
        self.store.record_intros(
            previous_commit.as_ref().map(|c| c.tree),
            tree,
            cid,
            cap.agent_id(),
        )?;
        match previous {
            Some(row) => {
                self.store.meta.cas_ref(
                    r#ref,
                    row.oid,
                    cid,
                    "commit",
                    cap.agent_id(),
                    cap.agent_id(),
                    false,
                )?;
            }
            None => {
                self.store.meta.insert_ref(
                    r#ref,
                    cid,
                    "commit",
                    false,
                    false,
                    cap.agent_id(),
                    "import",
                )?;
            }
        }
'''
new = '''        let cid = self.store.put_commit(&commit)?;
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
'''
if old not in api:
    raise SystemExit("import provenance anchor changed")
api = api.replace(old, new, 1)
api_p.write_text(api)

# Update session tests for the new session CAS signature and add a coupled rollback regression.
test_p = Path("crates/forge-store/tests/session_atomicity.rs")
t = test_p.read_text()
t = t.replace('"ns", "/")', '"ns", "/", &[])')
if "provenance_failure_rolls_back_checkin_publication" not in t:
    t += r'''

#[test]
fn provenance_failure_rolls_back_checkin_publication() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(1));
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_intro BEFORE INSERT ON object_intro BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta
        .cas_ref_session("shared", oid(1), oid(2), "a", "a", "ns", "/", &[oid(7)])
        .is_err());
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(1));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(1)));
    assert_eq!(meta.overlay_list("ns", "/").unwrap().len(), 1);
    assert_eq!(meta.observations("ns").unwrap().len(), 1);
    assert!(meta.intro_get(oid(7)).unwrap().is_none());
}

#[test]
fn generic_cas_provenance_failure_rolls_back_ref() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    meta.insert_ref("shared", oid(1), "commit", false, false, "a", "seed").unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_intro BEFORE INSERT ON object_intro BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta
        .cas_ref_with_intros("shared", oid(1), oid(2), "commit", "a", "a", false, &[oid(7)])
        .is_err());
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(1));
    assert!(meta.intro_get(oid(7)).unwrap().is_none());
}
'''
test_p.write_text(t)
