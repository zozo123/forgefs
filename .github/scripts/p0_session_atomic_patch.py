from pathlib import Path

meta_p = Path("crates/forge-store/src/meta.rs")
meta = meta_p.read_text()
if "pub fn create_session(" in meta:
    raise SystemExit("session atomicity already applied")

# Add atomic session creation before the old primitive insert_namespace method.
anchor = "    pub fn insert_namespace(\n"
pos = meta.index(anchor)
create_session = '''    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        id: &str,
        agent_id: &str,
        pinned: ObjectId,
        live_ref: &str,
        mount_main: bool,
    ) -> Result<()> {
        validate_ref_kind(live_ref, "commit")?;
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let ts = now_ms() as i64;
        tx.execute(
            "INSERT INTO namespaces (id, agent_id, created_ms, pinned_oid, live_ref) VALUES (?1,?2,?3,?4,?5)",
            params![id, agent_id, ts, pinned.as_bytes().as_slice(), live_ref],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,'commit',0,0,?3)",
            params![live_ref, pinned.as_bytes().as_slice(), ts],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,NULL,?2,?3,'session',?4)",
            params![live_ref, pinned.as_bytes().as_slice(), agent_id, ts],
        )
        .map_err(map_sql)?;
        let root_spec = format!("ref:{live_ref}");
        tx.execute(
            "INSERT INTO mounts (ns_id, path, spec, mode) VALUES (?1,'/',?2,'rw')",
            params![id, root_spec],
        )
        .map_err(map_sql)?;
        if mount_main {
            tx.execute(
                "INSERT INTO mounts (ns_id, path, spec, mode) VALUES (?1,'/main','ref:main','ro')",
                [id],
            )
            .map_err(map_sql)?;
        }
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

'''
meta = meta[:pos] + create_session + meta[pos:]

# Add atomic checkin/fork state transition before list_refs.
pos = meta.index("    pub fn list_refs(&self) -> Result<Vec<RefRow>> {")
checkin = '''    #[allow(clippy::too_many_arguments)]
    pub fn cas_ref_session(
        &self,
        name: &str,
        expected: ObjectId,
        new: ObjectId,
        agent_id: &str,
        fork_agent: &str,
        ns_id: &str,
        mount_path: &str,
    ) -> Result<CasResult> {
        validate_ref_kind(name, "commit")?;
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let row = tx
            .query_row(
                "SELECT oid, kind, protected, sealed FROM refs WHERE name=?1",
                [name],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )
            .optional()
            .map_err(map_sql)?
            .ok_or_else(|| Error::NotFound(format!("ref {name}")))?;
        let (oid_b, kind, protected, sealed) = row;
        if kind != "commit" {
            return Err(Error::Invalid(format!("ref {name} is {kind}, not commit")));
        }
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if protected != 0 {
            return Err(Error::Denied(format!("ref {name} is protected; session checkin cannot advance it")));
        }
        let current = oid_from_blob(oid_b)?;
        let ts = now_ms() as i64;

        let result = if current == expected {
            let n = tx
                .execute(
                    "UPDATE refs SET oid=?1, updated_ms=?2 WHERE name=?3 AND oid=?4 AND kind='commit' AND sealed=0 AND protected=0",
                    params![new.as_bytes().as_slice(), ts, name, expected.as_bytes().as_slice()],
                )
                .map_err(map_sql)?;
            if n != 1 {
                return Err(Error::Busy(format!("ref {name} changed during checkin")));
            }
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'cas',?5)",
                params![name, expected.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
            CasResult::Updated { name: name.to_string(), oid: new }
        } else {
            let fork = format!(
                "forks/{}/{}/{}",
                name,
                sanitize_agent(fork_agent),
                ulid::Ulid::new()
            );
            validate_ref_kind(&fork, "commit")?;
            tx.execute(
                "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,'commit',0,0,?3)",
                params![fork, new.as_bytes().as_slice(), ts],
            )
            .map_err(map_sql)?;
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'fork',?5)",
                params![fork, current.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
            let root_spec = format!("ref:{fork}");
            let n = tx
                .execute(
                    "UPDATE mounts SET spec=?1 WHERE ns_id=?2 AND path=?3",
                    params![root_spec, ns_id, mount_path],
                )
                .map_err(map_sql)?;
            if n != 1 {
                return Err(Error::Corrupt(format!("missing checkin mount {ns_id}:{mount_path}")));
            }
            CasResult::Forked {
                requested: name.to_string(),
                fork,
                ours: new,
                theirs: current,
            }
        };

        tx.execute(
            "DELETE FROM overlay WHERE ns_id=?1 AND mount=?2",
            params![ns_id, mount_path],
        )
        .map_err(map_sql)?;
        let n = tx
            .execute(
                "UPDATE namespaces SET pinned_oid=?1 WHERE id=?2",
                params![new.as_bytes().as_slice(), ns_id],
            )
            .map_err(map_sql)?;
        if n != 1 {
            return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
        }
        tx.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(result)
    }

    pub fn complete_noop_session(
        &self,
        ns_id: &str,
        mount_path: &str,
        pinned: ObjectId,
    ) -> Result<()> {
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        tx.execute(
            "DELETE FROM overlay WHERE ns_id=?1 AND mount=?2",
            params![ns_id, mount_path],
        )
        .map_err(map_sql)?;
        let n = tx
            .execute(
                "UPDATE namespaces SET pinned_oid=?1 WHERE id=?2",
                params![pinned.as_bytes().as_slice(), ns_id],
            )
            .map_err(map_sql)?;
        if n != 1 {
            return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
        }
        tx.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

'''
meta = meta[:pos] + checkin + meta[pos:]

# Batch provenance introductions atomically.
pos = meta.index("    pub fn intro_get(&self, oid: ObjectId) -> Result<Option<String>> {")
intro_many = '''    pub fn intro_insert_many(
        &self,
        oids: &[ObjectId],
        commit: ObjectId,
        agent_id: &str,
    ) -> Result<()> {
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let ts = now_ms() as i64;
        for oid in oids {
            tx.execute(
                "INSERT OR IGNORE INTO object_intro (oid, commit_oid, agent_id, ts_ms) VALUES (?1,?2,?3,?4)",
                params![oid.as_bytes().as_slice(), commit.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
        }
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

'''
meta = meta[:pos] + intro_many + meta[pos:]
meta_p.write_text(meta)

# Refactor Store provenance walk to collect first, then publish metadata once.
store_p = Path("crates/forge-store/src/lib.rs")
store = store_p.read_text()
old = '''        self.intro_walk(old, new, ObjectType::Tree, commit, agent)
    }

    fn intro_walk(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        expected: ObjectType,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {'''
new = '''        let mut oids = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut oids)?;
        self.meta.intro_insert_many(&oids, commit, agent)
    }

    fn intro_walk(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        expected: ObjectType,
        oids: &mut Vec<ObjectId>,
    ) -> Result<()> {'''
if old not in store:
    raise SystemExit("record_intros signature anchor changed")
store = store.replace(old, new, 1)
old = "        self.meta.intro_insert(new, commit, agent)?;"
if old not in store:
    raise SystemExit("intro insert anchor changed")
store = store.replace(old, "        oids.push(new);", 1)
old = "                    self.intro_walk(old_id, e.id, expected, commit, agent)?;"
if old not in store:
    raise SystemExit("intro recursion anchor changed")
store = store.replace(old, "                    self.intro_walk(old_id, e.id, expected, oids)?;", 1)
store_p.write_text(store)

# Wire the API to the new atomic metadata transitions.
api_p = Path("crates/forge-api/src/lib.rs")
api = api_p.read_text()
ss = api.index("    pub fn session_open(&self, cap: &Cap, from: &str) -> Result<String> {")
se = api.index("\n    pub fn mount(", ss)
session = '''    pub fn session_open(&self, cap: &Cap, from: &str) -> Result<String> {
        self.check_spec_read(cap, from)?;
        let (cid, _) = self.peel_commit(from)?;
        let ns_id = ulid::Ulid::new().to_string();
        let agent = sanitize_agent(cap.agent_id());
        let live = format!("heads/agents/{agent}/{ns_id}");
        self.check(cap, Op::Branch, Some(&live))?;
        let mount_main = cap.allows(Op::Read, Some("main"), now_ms()).is_ok();
        self.store
            .meta
            .create_session(&ns_id, cap.agent_id(), cid, &live, mount_main)?;
        Ok(ns_id)
    }
'''
api = api[:ss] + session + api[se:]

cs = api.index("    pub fn checkin(&self, cap: &Cap, ns: &str, mount: &str, msg: &str) -> Result<CasResult> {")
noop = api.index("        if new_tree == base_commit.tree && pin == row.oid {", cs)
noop_end = api.index("        let commit = Commit {", noop)
noop_block = '''        if new_tree == base_commit.tree && pin == row.oid {
            self.store.meta.complete_noop_session(ns, &m.path, pin)?;
            return Ok(CasResult::Noop {
                name: ref_name,
                oid: row.oid,
            });
        }
'''
api = api[:noop] + noop_block + api[noop_end:]

start = api.index("        let result = self.store.meta.cas_ref(", cs)
end = api.index("        Ok(result)", start)
atomic = '''        let result = self.store.meta.cas_ref_session(
            &ref_name,
            pin,
            cid,
            cap.agent_id(),
            cap.agent_id(),
            ns,
            &m.path,
        )?;
'''
api = api[:start] + atomic + api[end:]
api_p.write_text(api)

# Regression tests for crash/failure atomicity.
Path("crates/forge-store/tests/session_atomicity.rs").write_text(r'''use forge_store::Meta;
use forge_types::{CasResult, ObjectId};
use rusqlite::Connection;
use tempfile::tempdir;

fn oid(n: u8) -> ObjectId { ObjectId([n; 32]) }

fn seed(meta: &Meta, ns: &str, ref_name: &str, pin: ObjectId, current: ObjectId) {
    meta.insert_ref(ref_name, current, "commit", false, false, "a", "seed").unwrap();
    meta.insert_namespace(ns, "a", pin, ref_name).unwrap();
    meta.insert_mount(ns, "/", &format!("ref:{ref_name}"), "rw").unwrap();
    meta.overlay_upsert(ns, "/", "x", Some(oid(9)), false).unwrap();
    meta.observe(ns, "/", "read", oid(8)).unwrap();
}

#[test]
fn session_creation_rolls_back_if_mount_insert_fails() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_mount BEFORE INSERT ON mounts BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    let live = "heads/agents/a/ns1";
    assert!(meta.create_session("ns1", "a", oid(1), live, true).is_err());
    assert!(meta.get_namespace("ns1").is_err());
    assert!(meta.get_ref(live).unwrap().is_none());
    assert!(meta.reflog(live, 10).unwrap().is_empty());
}

#[test]
fn failed_checkin_cleanup_rolls_back_ref_and_session_state() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(1));
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_overlay BEFORE DELETE ON overlay BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta.cas_ref_session("shared", oid(1), oid(2), "a", "a", "ns", "/").is_err());
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(1));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(1)));
    assert_eq!(meta.overlay_list("ns", "/").unwrap().len(), 1);
    assert_eq!(meta.observations("ns").unwrap().len(), 1);
}

#[test]
fn successful_checkin_moves_ref_and_clears_session_state_together() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(1));
    let r = meta.cas_ref_session("shared", oid(1), oid(2), "a", "a", "ns", "/").unwrap();
    assert!(matches!(r, CasResult::Updated { .. }));
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(2));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(2)));
    assert!(meta.overlay_list("ns", "/").unwrap().is_empty());
    assert!(meta.observations("ns").unwrap().is_empty());
}

#[test]
fn stale_checkin_forks_and_retargets_mount_atomically() {
    let d = tempdir().unwrap();
    let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
    seed(&meta, "ns", "shared", oid(1), oid(2));
    let r = meta.cas_ref_session("shared", oid(1), oid(3), "a", "a", "ns", "/").unwrap();
    let CasResult::Forked { fork, .. } = r else { panic!("expected fork") };
    assert_eq!(meta.get_ref("shared").unwrap().unwrap().oid, oid(2));
    assert_eq!(meta.get_ref(&fork).unwrap().unwrap().oid, oid(3));
    assert_eq!(meta.get_namespace("ns").unwrap().pinned_oid, Some(oid(3)));
    let mounts = meta.list_mounts("ns").unwrap();
    assert_eq!(mounts.iter().find(|m| m.path == "/").unwrap().spec, format!("ref:{fork}"));
    assert!(meta.overlay_list("ns", "/").unwrap().is_empty());
    assert!(meta.observations("ns").unwrap().is_empty());
}

#[test]
fn provenance_batch_rolls_back_all_rows_on_failure() {
    let d = tempdir().unwrap();
    let db = d.path().join("m.sqlite");
    let meta = Meta::open(&db).unwrap();
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TRIGGER fail_second_intro BEFORE INSERT ON object_intro WHEN (SELECT count(*) FROM object_intro) >= 1 BEGIN SELECT RAISE(FAIL, 'boom'); END;"
    ).unwrap();
    assert!(meta.intro_insert_many(&[oid(1), oid(2)], oid(3), "a").is_err());
    assert!(meta.intro_get(oid(1)).unwrap().is_none());
    assert!(meta.intro_get(oid(2)).unwrap().is_none());
}
''')
