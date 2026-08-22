from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one anchor, found {n}")
    return text.replace(old, new, 1)


def replace_block(text: str, start: str, end: str, new: str, label: str) -> str:
    a = text.find(start)
    if a < 0:
        raise SystemExit(f"{label}: start missing")
    b = text.find(end, a)
    if b < 0:
        raise SystemExit(f"{label}: end missing")
    return text[:a] + new + text[b:]


# API: public trust metadata and compound session/checkin transitions.
p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '        fs::write(root.join("keys/seal.pub"), pk)?;',
    '        write_public(root.join("keys/seal.pub"), &pk)?;',
    "durable public seal key",
)
s = replace_once(
    s,
    "        store.meta.set_cap_root(&hmac_key, &pk)?;",
    "        store.meta.set_cap_root(&pk)?;",
    "public-only cap root",
)
s = replace_once(
    s,
    '''        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let store = Store::open(&root)?;
        let sk = SigningKey::from_bytes(&seal_seed);
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk: sk.verifying_key().to_bytes(),
            root,
        })''',
    '''        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let sk = SigningKey::from_bytes(&seal_seed);
        let seal_pk = sk.verifying_key().to_bytes();
        let store = Store::open(&root)?;
        let configured_pk = store.meta.get_seal_pub()?;
        if configured_pk != seal_pk.to_vec() {
            return Err(Error::Corrupt(
                "configured seal public key does not match local signing key".into(),
            ));
        }
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk,
            root,
        })''',
    "open verifies configured public key",
)

session = '''    pub fn session_open(&self, cap: &Cap, from: &str) -> Result<String> {
        self.check_spec_read(cap, from)?;
        let (cid, _) = self.peel_commit(from)?;
        let ns_id = ulid::Ulid::new().to_string();
        let agent = sanitize_agent(cap.agent_id());
        let live = format!("heads/agents/{agent}/{ns_id}");
        self.check(cap, Op::Branch, Some(&live))?;
        let include_main = cap.allows(Op::Read, Some("main"), now_ms()).is_ok();
        self.store
            .meta
            .create_session(&ns_id, cap.agent_id(), cid, &live, include_main)?;
        Ok(ns_id)
    }

'''
s = replace_block(s, "    pub fn session_open", "    pub fn mount", session, "session_open")

old = '''        let result = self.store.meta.cas_ref(
            &ref_name,
            pin,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
            false,
        )?;
        match &result {
            CasResult::Updated { .. } | CasResult::Forked { .. } => {
                self.store.meta.overlay_clear(ns, &m.path)?;
            }
            CasResult::Noop { .. } => {}
        }
        if let CasResult::Forked { fork, ours, .. } = &result {
            self.store
                .meta
                .update_mount_spec(ns, &m.path, &format!("ref:{fork}"))?;
            self.store.meta.set_pin(ns, *ours)?;
            self.store.meta.observations_clear(ns)?;
        }
        if let CasResult::Updated { oid, .. } = &result {
            self.store.meta.set_pin(ns, *oid)?;
            self.store.meta.observations_clear(ns)?;
        }
        Ok(result)'''
new = '''        self.store.meta.cas_ref_session(
            ns,
            &m.path,
            &ref_name,
            pin,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
        )'''
s = replace_once(s, old, new, "checkin compound transaction")

helper_anchor = '''fn sync_dir(path: &Path) -> Result<()> {
'''
write_public = '''fn write_public(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

'''
pos = s.find(helper_anchor)
if pos < 0:
    raise SystemExit("sync_dir helper missing")
s = s[:pos] + write_public + s[pos:]
p.write_text(s)


# Metadata: mutable DB is not a secret store; session/checkin are single transactions.
p = Path("crates/forge-store/src/meta.rs")
s = p.read_text()
s = replace_once(s, "  hmac_key BLOB NOT NULL,", "  hmac_key BLOB NOT NULL DEFAULT X'',", "cap root schema")
s = replace_once(
    s,
    '''        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        Ok(Self {''',
    '''        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        conn.execute(
            "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
            [],
        )
        .map_err(map_sql)?;
        Ok(Self {''',
    "legacy root HMAC scrub",
)
s = replace_block(
    s,
    "    pub fn set_cap_root",
    "    pub fn get_ref",
    '''    pub fn set_cap_root(&self, seal_pub: &[u8]) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO cap_root (id, hmac_key, seal_pub) VALUES (1, X'', ?1)",
            params![seal_pub],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn get_seal_pub(&self) -> Result<Vec<u8>> {
        let conn = self.write.lock();
        conn.query_row("SELECT seal_pub FROM cap_root WHERE id=1", [], |r| r.get(0))
            .map_err(|_| Error::Corrupt("missing cap_root".into()))
    }

''',
    "cap_root API",
)

create_session = '''    pub fn create_session(
        &self,
        id: &str,
        agent_id: &str,
        pinned: ObjectId,
        live_ref: &str,
        include_main: bool,
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
        tx.execute(
            "INSERT INTO mounts (ns_id, path, spec, mode) VALUES (?1,'/',?2,'rw')",
            params![id, format!("ref:{live_ref}")],
        )
        .map_err(map_sql)?;
        if include_main {
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
pos = s.find("    pub fn insert_namespace")
if pos < 0:
    raise SystemExit("insert_namespace missing")
s = s[:pos] + create_session + s[pos:]

cas_session = '''    #[allow(clippy::too_many_arguments)]
    pub fn cas_ref_session(
        &self,
        ns_id: &str,
        mount_path: &str,
        name: &str,
        expected: ObjectId,
        new: ObjectId,
        kind: &str,
        agent_id: &str,
        fork_agent: &str,
    ) -> Result<CasResult> {
        validate_ref_kind(name, kind)?;
        if name.starts_with("tags/") {
            return Err(Error::Denied("sealed tags cannot be updated through CAS".into()));
        }
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let (oid_b, current_kind, protected, sealed) = tx
            .query_row(
                "SELECT oid, kind, protected, sealed FROM refs WHERE name=?1",
                [name],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )
            .optional()
            .map_err(map_sql)?
            .ok_or_else(|| Error::NotFound(format!("ref {name}")))?;
        if current_kind != kind {
            return Err(Error::Invalid(format!(
                "ref {name} kind is immutable: {current_kind} != {kind}"
            )));
        }
        let current = oid_from_blob(oid_b)?;
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if protected != 0 {
            return Err(Error::Denied(format!("ref {name} is protected")));
        }
        let ts = now_ms() as i64;
        let updated = tx
            .execute(
                "UPDATE refs SET oid=?1, updated_ms=?2 WHERE name=?3 AND oid=?4 AND kind=?5 AND sealed=0 AND protected=0",
                params![new.as_bytes().as_slice(), ts, name, expected.as_bytes().as_slice(), kind],
            )
            .map_err(map_sql)?;
        let result = if updated == 1 {
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'cas',?5)",
                params![name, expected.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
            let n = tx
                .execute(
                    "UPDATE namespaces SET pinned_oid=?1, live_ref=?2 WHERE id=?3",
                    params![new.as_bytes().as_slice(), name, ns_id],
                )
                .map_err(map_sql)?;
            if n != 1 {
                return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
            }
            CasResult::Updated { name: name.to_string(), oid: new }
        } else {
            let fork = format!(
                "forks/{}/{}/{}",
                name,
                sanitize_agent(fork_agent),
                ulid::Ulid::new()
            );
            validate_ref_kind(&fork, kind)?;
            tx.execute(
                "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,0,0,?4)",
                params![fork, new.as_bytes().as_slice(), kind, ts],
            )
            .map_err(map_sql)?;
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'fork',?5)",
                params![fork, current.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
            let spec = format!("ref:{fork}");
            let m = tx
                .execute(
                    "UPDATE mounts SET spec=?1 WHERE ns_id=?2 AND path=?3",
                    params![spec, ns_id, mount_path],
                )
                .map_err(map_sql)?;
            if m != 1 {
                return Err(Error::Corrupt(format!("missing session mount {ns_id}:{mount_path}")));
            }
            let n = tx
                .execute(
                    "UPDATE namespaces SET pinned_oid=?1, live_ref=?2 WHERE id=?3",
                    params![new.as_bytes().as_slice(), fork, ns_id],
                )
                .map_err(map_sql)?;
            if n != 1 {
                return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
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
        tx.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(result)
    }

'''
pos = s.find("    pub fn list_refs")
if pos < 0:
    raise SystemExit("list_refs missing")
s = s[:pos] + cas_session + s[pos:]

s = replace_block(
    s,
    "    pub fn intro_insert",
    "    pub fn intro_get",
    '''    pub fn intro_insert(&self, oid: ObjectId, commit: ObjectId, agent_id: &str) -> Result<()> {
        self.intro_insert_many(&[oid], commit, agent_id)
    }

    pub fn intro_insert_many(
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

''',
    "intro batch",
)
p.write_text(s)


# Store provenance batching and typed old-edge validation.
p = Path("crates/forge-store/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''    ) -> Result<()> {
        self.intro_walk(old, new, ObjectType::Tree, commit, agent)
    }

    fn intro_walk(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        expected: ObjectType,
        commit: ObjectId,
        agent: &str,
    ) -> Result<()> {''',
    '''    ) -> Result<()> {
        let mut introduced = Vec::new();
        self.intro_walk(old, new, ObjectType::Tree, &mut introduced)?;
        self.meta.intro_insert_many(&introduced, commit, agent)
    }

    fn intro_walk(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        expected: ObjectType,
        introduced: &mut Vec<ObjectId>,
    ) -> Result<()> {''',
    "collect provenance",
)
s = replace_once(s, "        self.meta.intro_insert(new, commit, agent)?;", "        introduced.push(new);", "collect oid")
s = replace_once(
    s,
    '''                            ObjectType::Tree => Some(Tree::decode(&old_bytes)?),
                            ObjectType::Blob => None,
                            other => {''',
    '''                            ObjectType::Tree => Some(Tree::decode(&old_bytes)?),
                            other => {''',
    "strict old tree type",
)
s = replace_once(
    s,
    "                    self.intro_walk(old_id, e.id, expected, commit, agent)?;",
    "                    self.intro_walk(old_id, e.id, expected, introduced)?;",
    "recursive provenance",
)
p.write_text(s)


# Merge must not turn decode/type corruption into semantic conflict.
p = Path("crates/forge-merge/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''    let our_tree = store.get_tree(ours).ok();
    let their_tree = store.get_tree(theirs).ok();
    let base_tree = match base {
        Some(id) => store.get_tree(id).ok(),
        None => None,
    };
    if our_tree.is_none() || their_tree.is_none() {
        conflicts.push(ConflictPath {
            path: prefix.to_string(),
            a: Some(ours),
            b: Some(theirs),
            base,
        });
        return Ok(ours);
    }
    let a = our_tree.unwrap();
    let b = their_tree.unwrap();
    let g = base_tree.unwrap_or_else(Tree::default);''',
    '''    let a = store.get_tree(ours)?;
    let b = store.get_tree(theirs)?;
    let g = match base {
        Some(id) => store.get_tree(id)?,
        None => Tree::default(),
    };''',
    "merge fail closed",
)
p.write_text(s)
