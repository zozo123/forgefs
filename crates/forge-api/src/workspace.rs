//! I8/I9 pinned workspaces, mounts, observations, overlays, and checkin.

use super::*;

impl Forge {
    pub fn session_open(&self, cap: &Cap, from: &str) -> Result<String> {
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

    pub fn mount(&self, cap: &Cap, ns: &str, path: &str, spec: &str, rw: bool) -> Result<()> {
        self.require_ns(cap, ns)?;
        let path = normalize_abs(path)?;
        let mode = if rw { "rw" } else { "ro" };
        if rw {
            if let Spec::Ref(n) = parse_spec(spec)? {
                self.check(cap, Op::Write, Some(&n))?;
            } else if cap.has_unrestricted_ref_scope() {
                self.check(cap, Op::Write, None)?;
            } else {
                return Err(Error::Denied(
                    "ref-scoped caps cannot mount raw oids read-write".into(),
                ));
            }
        } else {
            self.check_spec_read(cap, spec)?;
        }
        // A mount naming something that does not resolve is bad input, not
        // durable state. Persisting one made `fsck --full` report MOUNT_REF
        // corruption (exit 2) on a repository whose bytes were entirely intact,
        // which any holder of read authority could trigger at will.
        self.resolve_spec_oid(spec)?;
        self.store.meta.insert_mount(ns, &path, spec, mode)?;
        Ok(())
    }

    fn mounts(&self, ns: &str) -> Result<Vec<Mount>> {
        Ok(self
            .store
            .meta
            .list_mounts(ns)?
            .into_iter()
            .map(Mount::from)
            .collect())
    }

    /// The tree a session actually sees through one of its mounts.
    ///
    /// I8 pins a session to one base OID, so a read through a read-write
    /// `ref:` mount must come from that base and never from the live ref,
    /// which other agents can move. Reading the live ref recorded an
    /// observation that `check_observations` then compared against the pinned
    /// tree, so the two could never agree: the session failed checkin with
    /// StaleObservation forever, no re-read or re-mount could clear it, and its
    /// staged work was published to no ref at all -- unlike the pure-writer
    /// path, which forks and preserves it.
    ///
    /// Foreign read-only mounts deliberately stay live. That is what makes
    /// cross-mount stale detection work, and `check_observations` validates
    /// those against the live tree to match.
    ///
    /// The default `/` mount is `ref:<live_ref>` on the session's own private
    /// ref, and `checkin` re-pins the session as it advances, so serving reads
    /// from the pin is also correct for the ordinary private-workspace case.
    fn session_mount_tree(&self, nsrow: &forge_store::NsRow, m: &Mount) -> Result<ObjectId> {
        if m.mode == Mode::Rw && matches!(parse_spec(&m.spec)?, Spec::Ref(_)) {
            if let Some(pin) = nsrow.pinned_oid {
                return Ok(self.store.get_commit(pin)?.tree);
            }
        }
        self.mount_tree(&m.spec)
    }

    fn mount_tree(&self, spec: &str) -> Result<ObjectId> {
        let oid = self.resolve_spec_oid(spec)?;
        match self.store.object_type(oid)? {
            ObjectType::Commit => Ok(self.store.get_commit(oid)?.tree),
            ObjectType::Snapshot => Ok(self.store.get_snapshot(oid)?.tree),
            ObjectType::Tree => Ok(oid),
            other => Err(Error::Invalid(format!("cannot mount {}", other.as_str()))),
        }
    }

    pub fn ls(
        &self,
        cap: &Cap,
        ns: &str,
        path: &str,
    ) -> Result<Vec<(String, String, String, bool)>> {
        let nsrow = self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let rel = rel_of(&m.path, path)?;
        let tree = self.session_mount_tree(&nsrow, m)?;
        let ov = self.store.meta.overlay_list(ns, &m.path)?;
        let ents = ns_ls(&self.store, &ov, tree, &rel)?;
        Ok(ents
            .into_iter()
            .map(|e| {
                (
                    e.name,
                    match e.kind {
                        forge_types::EntryKind::Blob => "blob".into(),
                        forge_types::EntryKind::Tree => "tree".into(),
                    },
                    e.id.hex(),
                    e.exec,
                )
            })
            .collect())
    }

    pub fn read(&self, cap: &Cap, ns: &str, path: &str) -> Result<Vec<u8>> {
        let nsrow = self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let ov = self.store.meta.overlay_list(ns, &m.path)?;
        let tree = self.session_mount_tree(&nsrow, m)?;
        match resolve(&self.store, &mounts, &ov, tree, path)? {
            Resolved::Blob { id, .. } => {
                let rel = rel_of(&m.path, path)?;
                self.store.meta.observe(ns, &m.path, &rel, id)?;
                self.store.get_blob_data(id)
            }
            Resolved::Tree(_) => Err(Error::Invalid("read of directory".into())),
        }
    }

    pub fn write(
        &self,
        cap: &Cap,
        ns: &str,
        path: &str,
        data: &[u8],
        exec: bool,
    ) -> Result<ObjectId> {
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        if m.mode != Mode::Rw {
            return Err(Error::Denied(format!("{} is read-only", m.path)));
        }
        if let Spec::Ref(n) = parse_spec(&m.spec)? {
            self.check(cap, Op::Write, Some(&n))?;
        } else {
            self.check(cap, Op::Write, None)?;
        }
        if data.len() as u64 > 64 * 1024 * 1024 {
            eprintln!("forge: warning blob {} bytes > 64MiB", data.len());
        }
        let id = self.store.put_blob_data(data)?;
        let rel = rel_of(&m.path, path)?;
        if rel.is_empty() {
            return Err(Error::Invalid("cannot write mount root".into()));
        }
        self.store
            .meta
            .overlay_upsert(ns, &m.path, &rel, Some(id), exec)?;
        Ok(id)
    }

    pub fn delete(&self, cap: &Cap, ns: &str, path: &str) -> Result<()> {
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        if m.mode != Mode::Rw {
            return Err(Error::Denied(format!("{} is read-only", m.path)));
        }
        if let Spec::Ref(n) = parse_spec(&m.spec)? {
            self.check(cap, Op::Write, Some(&n))?;
        } else {
            self.check(cap, Op::Write, None)?;
        }
        let rel = rel_of(&m.path, path)?;
        if rel.is_empty() {
            return Err(Error::Invalid("cannot delete mount root".into()));
        }
        self.store
            .meta
            .overlay_upsert(ns, &m.path, &rel, None, false)?;
        Ok(())
    }

    pub fn checkin(&self, cap: &Cap, ns: &str, mount: &str, msg: &str) -> Result<CasResult> {
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, mount)?;
        if m.mode != Mode::Rw {
            return Err(Error::Denied("checkin on ro mount".into()));
        }
        let ref_name = match parse_spec(&m.spec)? {
            Spec::Ref(n) => n,
            Spec::Oid(_) => return Err(Error::Invalid("cannot checkin an oid mount".into())),
        };
        self.check(cap, Op::Write, Some(&ref_name))?;
        let nsrow = self.store.meta.get_namespace(ns)?;
        let pin = nsrow.pinned_oid.ok_or(Error::InvalidBase)?;
        let row = self
            .store
            .meta
            .get_ref(&ref_name)?
            .ok_or_else(|| Error::NotFound(ref_name.clone()))?;
        let base_commit = self.store.get_commit(pin)?;
        let ov_rows = self.store.meta.overlay_list(ns, &m.path)?;
        let observations = self.store.meta.observations(ns)?;
        let ov = overlay_map(&ov_rows);
        self.check_observations(ns, &m.path, &ov, pin, &mounts)?;
        let batch = self.store.begin_publish_batch();
        let new_tree = apply_overlay(Some(base_commit.tree), &ov, &batch)?;
        if new_tree == base_commit.tree && pin == row.oid {
            batch.finish()?;
            self.store.meta.complete_noop_session(ns, &m.path, pin)?;
            return Ok(CasResult::Noop {
                name: ref_name,
                oid: row.oid,
            });
        }
        let mut reads = observations
            .into_iter()
            .map(|obs| ContributionRead {
                path: contribution_path(&obs.mount, &obs.path),
                id: obs.oid,
            })
            .collect::<Vec<_>>();
        reads.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        for pair in reads.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(Error::Invalid(format!(
                    "ambiguous contribution read path {}",
                    pair[0].path
                )));
            }
        }
        let mut writes = ov_rows
            .iter()
            .map(|row| contribution_path(&m.path, &row.path))
            .collect::<Vec<_>>();
        writes.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        writes.dedup();

        let ts = now_ms();
        let contribution = Contribution {
            base: pin,
            tree: new_tree,
            parents: vec![pin],
            reads,
            writes,
            agent: cap.agent_id().into(),
            ts,
        };
        let contribution_oid = batch.put_contribution(&contribution)?;
        let commit = Commit {
            tree: new_tree,
            parents: vec![pin],
            agent: cap.agent_id().into(),
            msg: msg.into(),
            ts,
            landmark: false,
            contrib: Some(contribution_oid),
        };
        let cid = batch.put_commit(&commit)?;
        // I4: metadata CAS is strictly after every referenced object's file and
        // containing directory entry is durable. Orphans before CAS are safe.
        batch.finish()?;
        let intro_oids = self
            .store
            .collect_intros(Some(base_commit.tree), new_tree)?;
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
        Ok(result)
    }

    fn check_observations(
        &self,
        ns: &str,
        checkin_mount: &str,
        ov: &forge_core::Overlay,
        pin: ObjectId,
        mounts: &[Mount],
    ) -> Result<()> {
        let pin_tree = self.store.get_commit(pin)?.tree;
        for obs in self.store.meta.observations(ns)? {
            if obs.mount == checkin_mount && ov.contains_key(&obs.path) {
                continue;
            }
            let tree = if obs.mount == checkin_mount {
                pin_tree
            } else if let Some(om) = mounts.iter().find(|m| m.path == obs.mount) {
                self.mount_tree(&om.spec)?
            } else {
                continue;
            };
            let now = blob_at(&self.store, tree, &obs.path)?;
            if now != Some(obs.oid) {
                self.stats.stale_observation.fetch_add(1, Ordering::Relaxed);
                return Err(Error::StaleObservation {
                    path: format!("{}:/{}", obs.mount, obs.path),
                    expected: obs.oid.hex(),
                    found: now.map(|id| id.hex()).unwrap_or_else(|| "missing".into()),
                });
            }
        }
        Ok(())
    }
}

fn contribution_path(mount: &str, rel: &str) -> String {
    if rel.is_empty() {
        return mount.to_string();
    }
    if mount == "/" {
        format!("/{rel}")
    } else {
        format!("{}/{}", mount.trim_end_matches('/'), rel)
    }
}

fn blob_at(store: &Store, tree: ObjectId, rel: &str) -> Result<Option<ObjectId>> {
    if rel.is_empty() {
        return Ok(None);
    }
    let parts = split_path(rel)?;
    let mut cur = tree;
    for (i, part) in parts.iter().enumerate() {
        let t = store.get_tree(cur)?;
        let Some(ent) = t.get(part) else {
            return Ok(None);
        };
        if i + 1 == parts.len() {
            return Ok(if ent.kind == EntryKind::Blob {
                Some(ent.id)
            } else {
                None
            });
        }
        if ent.kind != EntryKind::Tree {
            return Ok(None);
        }
        cur = ent.id;
    }
    Ok(None)
}
