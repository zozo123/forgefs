//! I8/I9 pinned workspaces, mounts, observations, overlays, and checkin.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::tree::{apply_overlay, split_path};
use forge_core::{now_ms, Commit, Contribution, ContributionRead};
use forge_ns::{
    longest_mount, ls as ns_ls, normalize_abs, overlay_map, parse_spec, rel_of, resolve, Mode,
    Mount, Resolved, Spec,
};
use forge_store::{sanitize_agent, Observed, Store};
use forge_types::{CasResult, EntryKind, Error, ObjectId, ObjectType, Result};
use std::sync::atomic::Ordering;

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
        // Counted only after the pin is durable metadata, so a denied or
        // failed open never inflates the session count.
        self.stats.sessions_opened.fetch_add(1, Ordering::Relaxed);
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
        // I9: a directory read is a read. Record what was listed before
        // returning it, including the case where nothing is there, so that
        // "this directory held exactly these entries" and "this path does not
        // exist" are checkable at checkin instead of silently unrecorded.
        let seen = observed_at(&self.store, &overlay_map(&ov), tree, &rel)?;
        self.store.meta.observe(ns, &m.path, &rel, seen)?;
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
        let rel = rel_of(&m.path, path)?;
        // I9: record the outcome of the lookup before acting on it. A miss used
        // to record nothing, which made "this path did not exist" invisible to
        // checkin and let another agent create it under the reader.
        let seen = observed_at(&self.store, &overlay_map(&ov), tree, &rel)?;
        self.store.meta.observe(ns, &m.path, &rel, seen)?;
        match resolve(&self.store, &mounts, &ov, tree, path)? {
            Resolved::Blob { id, .. } => self.store.get_blob_data(id),
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

    /// Publish one mount's staged overlay onto that mount's ref.
    ///
    /// Checkin publishes exactly ONE mount -- the one named -- and refuses
    /// outright when the session holds staged work under any other mount
    /// (#326, I19). Publishing every read-write mount was the other candidate
    /// and was rejected: each mount names its own ref, so "check in
    /// everything" is N independent ref CASes with no atomicity across them.
    /// One of them updating while the next forks is a half-published session
    /// that no single `CasResult`, exit code or Contribution receipt can
    /// describe, and the VERSION 1 receipt is frozen at one `base`, one `tree`
    /// and one parent list, so a multi-ref checkin is not representable
    /// without a format change and its own gate. Implicitly advancing refs the
    /// caller never named is also the wrong default for an isolation
    /// substrate: a stray `--rw` mount on a shared ref would be published by a
    /// bare `forge checkin`.
    ///
    /// What is indefensible is the third behaviour, the one this had: answer
    /// `noop` with exit 0 while the session held work checkin never folded.
    /// That is indistinguishable from "there was nothing to do", and
    /// `abandon_session` -- which counts overlay rows across the whole
    /// namespace -- then refused to retire the session, so the only way out of
    /// the loop was to discard the work.
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
        // #326: never report an outcome over work this checkin did not fold.
        // `overlay_mounts_outside` asks exactly what `abandon_session` asks --
        // overlay rows anywhere under this namespace -- so checkin and abandon
        // can no longer disagree about whether the session holds staged work.
        // Error::Invalid is exit 1 in CLI_ABI.md, the same row abandon already
        // uses for "the session holds staged work": the request is
        // unsatisfiable as stated and no retry of it will ever succeed.
        let stranded = self.store.meta.overlay_mounts_outside(ns, &m.path)?;
        if !stranded.is_empty() {
            let named = stranded
                .iter()
                .map(|(other, n)| {
                    let noun = if *n == 1 { "entry" } else { "entries" };
                    format!("{other} ({n} staged {noun})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Invalid(format!(
                "session {ns} holds staged work under mounts that checkin {} does not publish: {named}; \
                 check each mount in on its own (checkin --mount <path>) or discard it with \
                 abandon session --discard-staged",
                m.path
            )));
        }
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
        // The frozen VERSION 1 Contribution receipt names blob reads only, and
        // the typed graph enforces that every read edge is a blob. Directory
        // and absent observations therefore stay in the mutable catalog, where
        // `check_observations` above already enforced them; widening the
        // receipt would be an object-format change with its own gate.
        let mut reads = observations
            .into_iter()
            .filter_map(|obs| match obs.seen {
                Observed::Blob(id) => Some(ContributionRead {
                    path: contribution_path(&obs.mount, &obs.path),
                    id,
                }),
                Observed::Tree(_) | Observed::Absent => None,
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
        crate::test_hooks::process_barrier("FORGEFS_TEST_CHECKIN_CAS_BARRIER", 2, "checkin CAS")?;
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
            if obs.mount == checkin_mount && overlay_shadows(ov, &obs.path) {
                continue;
            }
            let tree = if obs.mount == checkin_mount {
                pin_tree
            } else if let Some(om) = mounts.iter().find(|m| m.path == obs.mount) {
                self.mount_tree(&om.spec)?
            } else {
                continue;
            };
            let now = current_at(&self.store, tree, &obs.path)?;
            if now != obs.seen {
                self.stats.stale_observation.fetch_add(1, Ordering::Relaxed);
                return Err(Error::StaleObservation {
                    path: format!("{}:/{}", obs.mount, obs.path),
                    expected: obs.seen.describe(),
                    found: now.describe(),
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

/// True when the session's own staged overlay decides `rel`, either directly or
/// through an ancestor it replaced or deleted. Such a path is the session's own
/// write, validated against its pin by `apply_overlay`, not a foreign read.
fn overlay_shadows(ov: &forge_core::Overlay, rel: &str) -> bool {
    if ov.contains_key(rel) {
        return true;
    }
    let mut prefix = rel;
    while let Some(cut) = prefix.rfind('/') {
        prefix = &prefix[..cut];
        if ov.contains_key(prefix) {
            return true;
        }
    }
    false
}

/// The entry `rel` names inside `tree`, or `None` when nothing is there.
fn entry_at(store: &Store, tree: ObjectId, rel: &str) -> Result<Option<(EntryKind, ObjectId)>> {
    let parts = split_path(rel)?;
    let mut cur = tree;
    for (i, part) in parts.iter().enumerate() {
        let t = store.get_tree(cur)?;
        let Some(ent) = t.get(part) else {
            return Ok(None);
        };
        if i + 1 == parts.len() {
            return Ok(Some((ent.kind, ent.id)));
        }
        if ent.kind != EntryKind::Tree {
            return Ok(None);
        }
        cur = ent.id;
    }
    Ok(None)
}

/// What `tree` holds at `rel` right now, in the same three-way vocabulary an
/// observation is recorded in. This is the checkin-side mirror of
/// `observed_at`; both must classify a path identically or every read would
/// look stale.
fn current_at(store: &Store, tree: ObjectId, rel: &str) -> Result<Observed> {
    if rel.is_empty() {
        return Ok(Observed::Tree(tree));
    }
    Ok(match entry_at(store, tree, rel)? {
        Some((EntryKind::Blob, id)) => Observed::Blob(id),
        Some((EntryKind::Tree, id)) => Observed::Tree(id),
        None => Observed::Absent,
    })
}

/// What a session read at `rel` through one mount, with its own overlay applied.
fn observed_at(
    store: &Store,
    ov: &forge_core::Overlay,
    tree: ObjectId,
    rel: &str,
) -> Result<Observed> {
    if !rel.is_empty() && overlay_shadows(ov, rel) {
        return Ok(match ov.get(rel) {
            Some(Some((id, _))) => Observed::Blob(*id),
            // A tombstone, or an ancestor the session replaced with a file or
            // deleted outright: nothing resolves here.
            Some(None) | None => Observed::Absent,
        });
    }
    current_at(store, tree, rel)
}

#[cfg(test)]
mod tests {
    use crate::Forge;
    use tempfile::tempdir;

    /// #308/I9: a read records what it saw. Reading the same path again sees
    /// the same thing, so the row is already correct and must not be rewritten.
    /// Measured with `sqlite3_total_changes`, not `MetaStats::txn_count`, which
    /// counts explicit transactions only and is blind to the autocommit
    /// `INSERT OR REPLACE` this path used to issue once per read.
    #[test]
    fn rereading_a_path_costs_exactly_one_row_mutation() {
        let dir = tempdir().unwrap();
        let forge = Forge::init(dir.path()).unwrap();
        let root = forge.root_cap().unwrap();
        let ns = forge.session_open(&root, "main").unwrap();
        forge.mount(&root, &ns, "/", "ref:main", true).unwrap();
        forge.write(&root, &ns, "/a.txt", b"v0", false).unwrap();

        let before = forge.store.meta.row_mutations();
        assert_eq!(forge.read(&root, &ns, "/a.txt").unwrap(), b"v0");
        let after_first = forge.store.meta.row_mutations();
        assert_eq!(forge.read(&root, &ns, "/a.txt").unwrap(), b"v0");
        let after_second = forge.store.meta.row_mutations();

        assert_eq!(
            after_first - before,
            1,
            "one read of a new path must record exactly one observation row"
        );
        assert_eq!(
            after_second - after_first,
            0,
            "the second read of an unchanged path rewrote its observation row"
        );

        // The observation I9 validates is still there, and there is one of it.
        let observations = forge.store.meta.observations(&ns).unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].path, "a.txt");

        // A directory listing is an observation too, and repeats the same way.
        let after_reads = forge.store.meta.row_mutations();
        forge.ls(&root, &ns, "/").unwrap();
        let after_first_ls = forge.store.meta.row_mutations();
        forge.ls(&root, &ns, "/").unwrap();
        assert_eq!(
            after_first_ls - after_reads,
            1,
            "the first listing of a directory must record one row"
        );
        assert_eq!(
            forge.store.meta.row_mutations(),
            after_first_ls,
            "the second listing of an unchanged directory rewrote its row"
        );

        // A path whose content moved is a different observation and is written.
        forge.write(&root, &ns, "/a.txt", b"v1", false).unwrap();
        let before_moved = forge.store.meta.row_mutations();
        assert_eq!(forge.read(&root, &ns, "/a.txt").unwrap(), b"v1");
        assert!(
            forge.store.meta.row_mutations() > before_moved,
            "a read that saw a new OID must record it"
        );
    }
}
