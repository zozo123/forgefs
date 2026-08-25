//! I8/I9/I19-I21 pinned workspaces, mounts, observations, overlays, and checkin.
//!
//! The pin belongs to the MOUNT, not the session. `session_mount_tree` is the
//! single place that answers "which tree does this mount show", and `read`,
//! `ls`, `check_observations` and `checkin` all go through it, so a read and its
//! later validation can never consult different trees.

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
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
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

    /// Take a mount, pinning a read-write one to the base it names (I19).
    ///
    /// A read-write mount records the commit its ref holds right now, and every
    /// later read, observation check and checkin through that mount resolves
    /// against THAT commit. Pinning per mount rather than per session is what
    /// lets a session hold read-write mounts on several refs at once: the pin
    /// belongs to the ref the mount actually names, so no read hits a live ref
    /// another agent can move (#233) and no read answers out of some other
    /// ref's tree.
    ///
    /// Read-only mounts deliberately stay live and take no pin. That is what
    /// makes cross-mount stale detection work: a read through one is meant to
    /// go stale when the ref moves, and `check_observations` validates it
    /// against the same live tree the read saw.
    pub fn mount(&self, cap: &Cap, ns: &str, path: &str, spec: &str, rw: bool) -> Result<()> {
        self.require_ns(cap, ns)?;
        let path = normalize_abs(path)?;
        let mode = if rw { "rw" } else { "ro" };
        if rw {
            match parse_spec(spec)? {
                Spec::Ref(n) => {
                    self.check(cap, Op::Write, Some(&n))?;
                    self.refuse_rw_mount_of_protected_ref(&n, spec, &path)?;
                }
                // An `oid:` spec names immutable bytes: there is no ref for
                // `checkin` to advance, and it refused such a mount
                // unconditionally, so a write through one was staged where no
                // verb and no capability could ever publish it while `abandon`
                // demanded a checkin the CLI could not perform. `fsck` already
                // reports the row as MOUNT_RW_OID corruption, so accepting the
                // mount also let a holder of write authority manufacture a
                // corruption finding on entirely intact bytes. Refusing the
                // mount is the honest end of that: no write path without a
                // publish path (I20).
                Spec::Oid(_) => {
                    return Err(Error::Denied(format!(
                        "cannot mount {spec} read-write at {path}: an oid: spec names immutable \
                         bytes with no ref to advance, so a write through it could never be \
                         published; mount it read-only, or mount the ref that carries it"
                    )))
                }
            }
        } else {
            self.check_spec_read(cap, spec)?;
        }
        // A mount naming something that does not resolve is bad input, not
        // durable state. Persisting one made `fsck --full` report MOUNT_REF
        // corruption (exit 2) on a repository whose bytes were entirely intact,
        // which any holder of read authority could trigger at will.
        let oid = self.resolve_spec_oid(spec)?;
        let object_type = self.store.object_type(oid)?;
        if rw && object_type != ObjectType::Commit {
            // The same rule as the `oid:` case, one step out: `checkin` CASes a
            // ref of kind `commit`, so a read-write mount of a ref holding a
            // snapshot or a bare tree is another write with no publish path.
            return Err(Error::Denied(format!(
                "cannot mount {spec} read-write at {path}: it names a {}, and only a commit ref \
                 can be advanced by checkin",
                object_type.as_str()
            )));
        }
        self.refuse_retargeting_staged_work(ns, &path, spec, rw)?;
        // I19: a read-write mount carries its own base; a read-only one takes
        // none, because resolving live is precisely what it is for.
        let base = if rw { Some(oid) } else { None };
        self.store.meta.insert_mount(ns, &path, spec, mode, base)?;
        Ok(())
    }

    /// I20: refuse a read-write mount of a PROTECTED ref, at mount time.
    ///
    /// The third and last shape of "a write path with no publish path". I5
    /// makes a protected ref deny every session CAS -- `cas_ref_session`
    /// answers `ref R is protected; session checkin cannot advance it` -- so
    /// write authority over one is authority `checkin` can never exercise.
    /// Accepting the mount staged writes into an overlay that `checkin` then
    /// denied and that `abandon` refused to retire, leaving `--discard-staged`
    /// -- which destroys the work -- as the only exit (#328). That is I20's own
    /// rule failing on the one shape it did not check, and I21's liveness with
    /// it.
    ///
    /// Refused with the same error and the same exit code as the read-write
    /// `oid:` spec and the non-commit ref above, because it is the same defect:
    /// knowably unpublishable when the mount is created, so the honest place to
    /// fail is the mount, not the checkin that discovers it later.
    ///
    /// A mount-time check closes this shape COMPLETELY, which is not obvious
    /// and is the reason it is enough on its own. `refs.protected` is
    /// write-once: the only statements that ever write a 1 into it are
    /// `insert_ref`, `insert_ref_with_intros` and `commit_seal`. The first two
    /// refuse a name that already exists, so neither can protect a ref a mount
    /// already names; `commit_seal` writes `tags/*` alone, which `insert_ref`
    /// forbids any commit ref from being called and which a read-write mount is
    /// already refused for holding a snapshot. Every fork path writes the
    /// literal 0, and the ref-advancing `UPDATE` does not mention the column at
    /// all. So protection cannot be added to a ref underneath a live mount, and
    /// this check is not a first line of defence but the whole line.
    /// `mount_protection.rs` holds that closure property against the public
    /// API, so a future verb that protects an existing ref fails there and is
    /// forced to decide what happens to the mounts already on it.
    ///
    /// A ref that does not exist is left to `resolve_spec_oid` below, so a
    /// missing spec keeps answering `NotFound` rather than "protected".
    fn refuse_rw_mount_of_protected_ref(&self, name: &str, spec: &str, path: &str) -> Result<()> {
        let Some(row) = self.store.meta.get_ref(name)? else {
            return Ok(());
        };
        if !row.protected {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "cannot mount {spec} read-write at {path}: ref {name} is protected, so session \
             checkin can never advance it and a write through this mount could never be \
             published; mount it read-only, or branch it and mount the branch"
        )))
    }

    /// Refuse a re-mount that would change where already-staged work lands.
    ///
    /// `insert_mount` is `INSERT OR REPLACE` on (ns, path) while the overlay is
    /// keyed on (ns, mount path), so before I19 re-mounting a path at a
    /// different ref silently re-aimed everything staged under it at that other
    /// ref: no refusal, no warning, no discard. The mount row now records the
    /// spec and the pin the overlay was staged against, which is what makes the
    /// collision detectable at all.
    ///
    /// Only a change that moves the work is refused. Re-mounting the same spec
    /// read-write is idempotent and stays legal; demoting a read-write mount
    /// that holds staged work to read-only is refused for the same reason as a
    /// spec change, because `checkin` refuses a read-only mount and the work
    /// would be stranded -- I18 keeps it readable, but nothing could publish it.
    fn refuse_retargeting_staged_work(
        &self,
        ns: &str,
        path: &str,
        spec: &str,
        rw: bool,
    ) -> Result<()> {
        let Some(existing) = self.store.meta.get_mount(ns, path)? else {
            return Ok(());
        };
        let retarget = existing.spec != spec;
        let demote = existing.mode == "rw" && !rw;
        if !retarget && !demote {
            return Ok(());
        }
        let staged = self.store.meta.overlay_list(ns, path)?.len();
        if staged == 0 {
            return Ok(());
        }
        let noun = if staged == 1 { "entry" } else { "entries" };
        let change = if retarget {
            format!("re-mounting it at {spec}")
        } else {
            "demoting it to read-only".to_string()
        };
        Err(Error::Invalid(format!(
            "mount {path} holds {staged} staged {noun} written against {}; {change} would send \
             that work somewhere it was never written for. Check the mount in, or discard it \
             with abandon session --discard-staged, before changing it",
            existing.spec
        )))
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
    /// I19: a read-write mount carries its OWN pinned base, so a read through it
    /// comes from that base and never from the live ref other agents can move.
    /// That is what #233 needed -- a read through a read-write mount recorded an
    /// observation against the live ref that `check_observations` then compared
    /// against the pinned tree, so the two could never agree and the session
    /// failed checkin with StaleObservation forever -- and the pin is now the
    /// one belonging to the ref THIS mount names, which is what a single
    /// session-wide pin got wrong: it served `ref:base`'s tree through a mount
    /// of `ref:other`, so a file present only in `other` read as absent and any
    /// read through it wedged the session the same way #233 did.
    ///
    /// Read-only mounts deliberately stay live and carry no pin. That is what
    /// makes cross-mount stale detection work, and `check_observations` routes
    /// through this same function so a read and its later validation can never
    /// consult different trees.
    ///
    /// A read-write mount with no pin is reachable only for a pre-v3 catalog row
    /// whose ref has since been deleted; `mount_tree` then fails closed with
    /// `NotFound`, which is also what `fsck` reports for it (MOUNT_REF).
    fn session_mount_tree(&self, m: &Mount) -> Result<ObjectId> {
        if m.mode == Mode::Rw {
            if let Some(base) = m.base_oid {
                return self.tree_of(base);
            }
        }
        self.mount_tree(&m.spec)
    }

    fn mount_tree(&self, spec: &str) -> Result<ObjectId> {
        self.tree_of(self.resolve_spec_oid(spec)?)
    }

    /// The tree a mountable object exposes.
    fn tree_of(&self, oid: ObjectId) -> Result<ObjectId> {
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
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let rel = rel_of(&m.path, path)?;
        let tree = self.session_mount_tree(m)?;
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
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let ov = self.store.meta.overlay_list(ns, &m.path)?;
        let tree = self.session_mount_tree(m)?;
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

    /// Fold one mount's overlay onto THAT MOUNT's pinned base and CAS the ref
    /// that mount names, from that pin (I19).
    ///
    /// The expected value is the mount's own pin, not the session's. A session
    /// pin was wrong for every mount but the session's own: checking in a mount
    /// of `ref:other` folded onto `ref:base`'s tree and then CASed `ref:other`
    /// from a commit that was never in `other`'s history, so `other` could end
    /// up holding `base`'s content. I5 is unchanged and still the loser's
    /// contract: if the named ref has moved off this mount's pin the CAS loses
    /// and forks, and the fork carries this mount's completed work (I18).
    ///
    /// Checkin publishes exactly ONE mount -- the one named. Publishing every
    /// read-write mount was the other candidate and was rejected: each mount
    /// names its own ref, so "check in everything" is N independent ref CASes
    /// with no atomicity across them. One of them updating while the next forks
    /// is a half-published session that no single `CasResult`, exit code or
    /// Contribution receipt can describe, and the VERSION 1 receipt is frozen
    /// at one `base`, one `tree` and one parent list, so a multi-ref checkin is
    /// not representable without a format change and its own gate. Implicitly
    /// advancing refs the caller never named is also the wrong default for an
    /// isolation substrate: a stray `--rw` mount on a shared ref would be
    /// published by a bare `forge checkin`.
    ///
    /// What is indefensible is the third behaviour, the one this had: answer
    /// `noop` with exit 0 while the session held work checkin never folded.
    /// That is indistinguishable from "there was nothing to do", and
    /// `abandon_session` -- which counts overlay rows across the whole
    /// namespace -- then refused to retire the session, so the only way out of
    /// the loop was to discard the work. I22 forbids exactly that sentence and
    /// nothing wider: the refusal is scoped to the `Noop` outcome. `updated`
    /// and `forked` are progress and may legitimately leave another mount
    /// staged -- under I19 a session holds a pin per writable mount and drains
    /// them one `--mount` at a time, so refusing those too would deny the very
    /// escape the diagnostic advises and wedge the session (I21).
    pub fn checkin(&self, cap: &Cap, ns: &str, mount: &str, msg: &str) -> Result<CasResult> {
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, mount)?;
        if m.mode != Mode::Rw {
            return Err(Error::Denied("checkin on ro mount".into()));
        }
        let ref_name = match parse_spec(&m.spec)? {
            Spec::Ref(n) => n,
            // Unreachable through `mount`, which refuses a read-write `oid:`
            // mount outright, and through a migrated catalog, which demotes any
            // such row to read-only. Kept as the fail-closed floor.
            Spec::Oid(_) => return Err(Error::Invalid("cannot checkin an oid mount".into())),
        };
        self.check(cap, Op::Write, Some(&ref_name))?;
        // I19: THIS MOUNT's pin, not the session's. `InvalidBase` is the same
        // answer a session with no pin got before: nothing to fold onto.
        let pin = m.base_oid.ok_or(Error::InvalidBase)?;
        let row = self
            .store
            .meta
            .get_ref(&ref_name)?
            .ok_or_else(|| Error::NotFound(ref_name.clone()))?;
        let base_commit = self.store.get_commit(pin)?;
        let ov_rows = self.store.meta.overlay_list(ns, &m.path)?;
        let observations = self.store.meta.observations(ns)?;
        let ov = overlay_map(&ov_rows);
        self.check_observations(ns, &mounts)?;
        let batch = self.store.begin_publish_batch();
        let new_tree = apply_overlay(Some(base_commit.tree), &ov, &batch)?;
        if new_tree == base_commit.tree && pin == row.oid {
            // I22: this mount stages nothing, so the only outcome left is
            // `Noop` -- and `Noop` is the one answer that may never be given
            // over work that exists. `overlay_mounts_outside` asks exactly what
            // `abandon_session` asks -- overlay rows anywhere under this
            // namespace -- so the two verbs can never disagree about whether
            // the session still holds staged work.
            //
            // Scoped to this branch on purpose. Refusing `updated`/`forked` as
            // well would wedge a session holding two writable mounts with work
            // in both (I19, I21): publishing either would be refused on account
            // of the other, and the escape this very diagnostic advises could
            // never be taken.
            //
            // `Error::Invalid` is exit 1 in CLI_ABI.md, the same row abandon
            // already uses for "the session holds staged work": the request as
            // stated is unsatisfiable and no retry of it will ever succeed.
            // Only work that EXISTS may block a "there was nothing to do".
            // An overlay entry that folds to its own mount's base -- a delete
            // of a path that mount does not have, a write of bytes already
            // there -- is a ROW, not work, and `abandon --discard` is the only
            // thing that ever cared about the difference.
            //
            // Counting rows instead wedges the session all of whose mounts
            // hold only such rows: every checkin lands here, every one refuses
            // on account of every other, `abandon` without a discard refuses
            // because rows exist, and the escape this diagnostic advises can
            // never be taken (I21). `model_composition.rs` found that; the
            // regression test is `checkin_staged_work.rs`.
            let stranded: Vec<(String, u64)> = self
                .store
                .meta
                .overlay_mounts_outside(ns, &m.path)?
                .into_iter()
                .map(|(other, count)| {
                    let real = self.mount_overlay_changes_its_base(ns, &other, &mounts, &batch)?;
                    Ok((other, count, real))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, _, real)| *real)
                .map(|(other, count, _)| (other, count))
                .collect();
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
                    "checkin {} has nothing to publish, but session {ns} holds staged work under \
                     mounts it does not publish: {named}; check each mount in on its own \
                     (checkin --mount <path>) or discard it with abandon session --discard-staged",
                    m.path
                )));
            }
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

    /// Does the overlay staged under `path` fold to anything other than that
    /// mount's own base?
    ///
    /// I22 asks whether the session holds WORK, not whether it holds rows, and
    /// the two differ exactly when an overlay entry reproduces what the base
    /// already has. Answering it costs one fold per other mount, on the `Noop`
    /// path only -- the path that was about to do nothing anyway.
    ///
    /// Fails SAFE in both unusual directions: a mount this session does not
    /// have, or a read-write mount with no pin, counts as holding work, so an
    /// unexpected shape produces a refusal that names it rather than a silent
    /// `Noop` over it.
    fn mount_overlay_changes_its_base(
        &self,
        ns: &str,
        path: &str,
        mounts: &[Mount],
        batch: &impl forge_core::tree::TreeStore,
    ) -> Result<bool> {
        let Some(om) = mounts.iter().find(|x| x.path == path) else {
            return Ok(true);
        };
        let Some(base) = om.base_oid else {
            return Ok(true);
        };
        let rows = self.store.meta.overlay_list(ns, path)?;
        if rows.is_empty() {
            return Ok(false);
        }
        let base_tree = self.store.get_commit(base)?.tree;
        let folded = apply_overlay(Some(base_tree), &overlay_map(&rows), batch)?;
        Ok(folded != base_tree)
    }

    /// Re-validate every recorded observation against the tree the mount that
    /// recorded it resolves to NOW.
    ///
    /// I19: this routes through `session_mount_tree`, the same function the read
    /// used, so a read and its validation can never consult different trees. A
    /// read-write mount is checked against its own pin, which cannot move under
    /// it, so an authorised read through one can never make a session
    /// unpublishable (I21); a read-only mount is checked live, which is exactly
    /// the cross-mount staleness I9 is for. Comparing a pinned read-write mount
    /// against its LIVE ref was the wedge: the two could never agree, and the
    /// diagnostic named the observation rather than the mount, so the escape was
    /// not derivable from it.
    fn check_observations(&self, ns: &str, mounts: &[Mount]) -> Result<()> {
        // Each observing mount's OWN staged overlay, loaded at most once.
        let mut overlays: BTreeMap<String, forge_core::Overlay> = BTreeMap::new();
        for obs in self.store.meta.observations(ns)? {
            let Some(om) = mounts.iter().find(|m| m.path == obs.mount) else {
                continue;
            };
            let mount_ov = match overlays.entry(om.path.clone()) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let rows = self.store.meta.overlay_list(ns, &om.path)?;
                    e.insert(overlay_map(&rows))
                }
            };
            // The session's own staged write decides this path. It is not a
            // foreign read, `apply_overlay` validates it against this mount's
            // pin at fold time, and the recorded observation is a reading of
            // the overlay as it stood at read time -- so comparing it against
            // the base tree, or against a LATER overlay, compares two different
            // trees and can never converge.
            //
            // The skip is per OBSERVING MOUNT. It used to apply only when the
            // observation belonged to the mount being checked in, so a session
            // holding two writable mounts wedged itself with one authorised
            // read: `write /a.txt` then `read /a.txt` recorded the overlay's
            // blob under `/`, and `checkin /w1` compared that against `/`'s
            // pinned tree, which does not have it. Every checkin of every other
            // mount then refused `StaleObservation` forever, no re-read could
            // clear it -- re-reading records the overlay's blob again -- and
            // `abandon` refused over the staged work, leaving `--discard-staged`
            // as the only exit. That is I21's "an authorised read can never make
            // the session's work unpublishable" failing exactly as #233 did, one
            // mount over. `model_composition.rs` found it; the regression test is
            // `observation_scope.rs`.
            if overlay_shadows(mount_ov, &obs.path) {
                continue;
            }
            let tree = self.session_mount_tree(om)?;
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
        // `session_open` already mounts `/` read-write on the session's own
        // live head, pinned to the commit `main` holds. This used to re-mount
        // it at `ref:main`, which I20 now refuses: `main` is protected, so no
        // checkin could ever advance it (#328). The session's own head is the
        // shape a real agent writes through anyway.
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
