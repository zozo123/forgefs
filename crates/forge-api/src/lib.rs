//! The native Forge API. Agents speak this; POSIX is an adapter.

mod bench;
mod export;
mod fsck;
mod serve;
mod soak;

pub use bench::{
    merge_all_and_seal, private_checkins, run as run_bench, shared_stampede, BenchReport,
};
pub use fsck::{FsckFinding, FsckReport};
pub use serve::{dispatch as dispatch_request, serve};
pub use soak::{private_checkins_bounded, run_bench_with_workers, shared_stampede_bounded};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use forge_cap::{attenuate, mint_integrator, mint_root, verify, Cap, Op};
use forge_core::cbor::{encode_map_sorted, encode_text};
use forge_core::tree::{apply_overlay, split_path};
use forge_core::{hash_bytes, now_ms, Blob, Commit, Conflict, Snapshot, Tree};
use forge_merge::{merge_bases, three_way, MergeOutcome};
use forge_ns::{
    longest_mount, ls as ns_ls, normalize_abs, overlay_map, parse_spec, rel_of, resolve, Mode,
    Mount, Resolved, Spec,
};
use forge_store::{sanitize_agent, Store};
use forge_types::{CasResult, EntryKind, Error, ObjectId, ObjectType, RefRow, Result};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApiStats {
    pub stale_observation: u64,
    pub merge_conflict: u64,
}

#[derive(Debug, Default)]
struct ApiCounters {
    stale_observation: AtomicU64,
    merge_conflict: AtomicU64,
}

pub struct Forge {
    store: Store,
    hmac_key: [u8; 32],
    seal_seed: [u8; 32],
    seal_pk: [u8; 32],
    root: PathBuf,
    stats: ApiCounters,
    // Shared for direct clients, exclusive for the daemon. The descriptor lifetime is the lock.
    _cell_lock: File,
    exclusive_cell_lock: bool,
}

impl Forge {
    pub fn init(dir: &Path) -> Result<Self> {
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
        fs::create_dir_all(parent)?;
        let base = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".forge");
        let staging = parent.join(format!(
            "{base}.init-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        fs::create_dir(&staging)?;

        let prepared = (|| -> Result<File> {
            secure_key_dir(&staging.join("keys"))?;
            fs::create_dir_all(staging.join("objects"))?;
            fs::create_dir_all(staging.join("tmp"))?;

            let mut hmac_key = [0u8; 32];
            let mut seal_seed = [0u8; 32];
            getrandom::getrandom(&mut hmac_key).map_err(|e| Error::Internal(e.to_string()))?;
            getrandom::getrandom(&mut seal_seed).map_err(|e| Error::Internal(e.to_string()))?;
            let sk = SigningKey::from_bytes(&seal_seed);
            let pk = sk.verifying_key().to_bytes();

            write_secret(staging.join("keys/root.secret"), &hmac_key)?;
            write_secret(staging.join("keys/seal.ed25519"), &seal_seed)?;
            write_public(staging.join("keys/seal.pub"), &pk)?;

            let store = Store::open(&staging)?;
            store.meta.set_cap_root(&pk)?;

            let root_cap = mint_root(&hmac_key)?;
            let integ = mint_integrator(&hmac_key)?;
            write_secret(
                staging.join("keys/root.cap"),
                root_cap.to_token().as_bytes(),
            )?;
            write_secret(
                staging.join("keys/integrator.cap"),
                integ.to_token().as_bytes(),
            )?;
            sync_dir(&staging.join("keys"))?;

            let empty = store.empty_tree_id()?;
            let commit = Commit {
                tree: empty,
                parents: vec![],
                agent: "init".into(),
                msg: "init".into(),
                ts: now_ms(),
                landmark: true,
            };
            let cid = store.put_commit(&commit)?;
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
            store.meta.landmark(cid, "commit", "init")?;
            drop(store);

            write_public(staging.join("VERSION"), b"1\n")?;
            // Acquire the direct-client shared ownership while LOCK is still in
            // staging. The descriptor keeps the same inode locked across rename,
            // eliminating the post-publication daemon race.
            let cell_lock = acquire_cell_lock(&staging, false)?;
            sync_dir(&staging)?;
            Ok(cell_lock)
        })();

        let cell_lock = match prepared {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };

        if let Err(error) = publish_noreplace(&staging, &root) {
            drop(cell_lock);
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = sync_dir(parent) {
            return Err(Error::Io(format!(
                "repository published at {} but parent directory fsync failed: {error}",
                root.display()
            )));
        }

        // Ownership was acquired before publication; opening the committed cell
        // cannot race a daemon. All repository contents were already validated in
        // staging, so remaining failures are local I/O/corruption, not ambiguity.
        Self::open_locked(root, cell_lock, false)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, false)
    }

    /// Open a cell for `forge serve`. The exclusive lock is acquired before
    /// SQLite or object state is opened, so a daemon can never coexist with a
    /// direct client that holds the shared cell lock.
    pub fn open_for_serve(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, true)
    }

    fn open_with_lock(dir: &Path, exclusive: bool) -> Result<Self> {
        let root = find_forge(dir)?;
        let cell_lock = acquire_cell_lock(&root, exclusive)?;
        Self::open_locked(root, cell_lock, exclusive)
    }

    fn open_locked(root: PathBuf, cell_lock: File, exclusive: bool) -> Result<Self> {
        validate_key_permissions(&root.join("keys"))?;
        let hmac = read32(&root.join("keys/root.secret"))?;
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
            stats: ApiCounters::default(),
            _cell_lock: cell_lock,
            exclusive_cell_lock: exclusive,
        })
    }

    pub(crate) fn has_exclusive_cell_lock(&self) -> bool {
        self.exclusive_cell_lock
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn api_stats(&self) -> ApiStats {
        ApiStats {
            stale_observation: self.stats.stale_observation.load(Ordering::Relaxed),
            merge_conflict: self.stats.merge_conflict.load(Ordering::Relaxed),
        }
    }

    pub fn load_cap(&self, token: &str) -> Result<Cap> {
        let cap = Cap::from_token(token.trim())?;
        verify(&self.hmac_key, &cap)?;
        Ok(cap)
    }

    pub fn root_cap(&self) -> Result<Cap> {
        let t = fs::read_to_string(self.root.join("keys/root.cap"))?;
        self.load_cap(t.trim())
    }

    pub fn integrator_cap(&self) -> Result<Cap> {
        let t = fs::read_to_string(self.root.join("keys/integrator.cap"))?;
        self.load_cap(t.trim())
    }

    fn check(&self, cap: &Cap, op: Op, r#ref: Option<&str>) -> Result<()> {
        verify(&self.hmac_key, cap)?;
        cap.allows(op, r#ref, now_ms())
    }

    /// Unrestricted ref scope is an explicit full-ref cap (root), not "no agent".
    fn ref_unrestricted(cap: &Cap) -> bool {
        cap.ref_sets.is_empty() && cap.op_ref_sets.is_empty()
    }

    fn require_ns(&self, cap: &Cap, ns: &str) -> Result<forge_store::NsRow> {
        let row = self.store.meta.get_namespace(ns)?;
        if row.agent_id == cap.agent_id() {
            Ok(row)
        } else {
            Err(Error::Denied(format!(
                "namespace {ns} is owned by {}, not {}",
                row.agent_id,
                cap.agent_id()
            )))
        }
    }

    fn check_spec_read(&self, cap: &Cap, spec: &str) -> Result<()> {
        match parse_spec(spec)? {
            Spec::Ref(n) => self.check(cap, Op::Read, Some(&n)),
            Spec::Oid(_) => {
                if Self::ref_unrestricted(cap) {
                    self.check(cap, Op::Read, None)
                } else {
                    Err(Error::Denied(
                        "ref-scoped caps cannot address raw object ids".into(),
                    ))
                }
            }
        }
    }

    pub fn grant(&self, cap: &Cap, extra: Vec<String>) -> Result<Cap> {
        self.check(cap, Op::Grant, None)?;
        attenuate(&self.hmac_key, cap, extra)
    }

    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let mut out = Vec::new();
        for r in self.store.meta.list_refs()? {
            if cap.allows(Op::Read, Some(&r.name), now_ms()).is_ok() {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Publish a sealed snapshot to a recipient-owned inbox ref.
    /// ForgeFS stores only the durable pointer; scheduling stays above the core.
    pub fn inbox_push(&self, cap: &Cap, to: &str, snapshot: &str) -> Result<CasResult> {
        let recipient = sanitize_agent(to);
        if recipient != to || recipient == "anon" {
            return Err(Error::Invalid(format!("invalid inbox recipient {to:?}")));
        }
        self.check_spec_read(cap, snapshot)?;
        let oid = self.resolve_spec_oid(snapshot)?;
        if self.store.object_type(oid)? != ObjectType::Snapshot {
            return Err(Error::Invalid(
                "inbox payload must be a sealed snapshot".into(),
            ));
        }
        let name = format!("inbox/{recipient}/{}", ulid::Ulid::new());
        self.check(cap, Op::Write, Some(&name))?;
        self.store.meta.cas_ref(
            &name,
            ObjectId::ZERO,
            oid,
            "snapshot",
            cap.agent_id(),
            cap.agent_id(),
            false,
        )
    }

    /// List only the calling agent's concrete inbox refs that its cap can read.
    pub fn inbox_list(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        let agent = cap.agent_id();
        if sanitize_agent(agent) != agent || agent == "anon" {
            return Err(Error::Invalid(format!("invalid inbox agent {agent:?}")));
        }
        let prefix = format!("inbox/{agent}/");
        let mut out = Vec::new();
        for row in self.store.meta.list_refs()? {
            if row.name.starts_with(&prefix)
                && cap.allows(Op::Read, Some(&row.name), now_ms()).is_ok()
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    pub fn peel_commit(&self, spec: &str) -> Result<(ObjectId, Commit)> {
        let oid = self.resolve_spec_oid(spec)?;
        match self.store.object_type(oid)? {
            ObjectType::Commit => Ok((oid, self.store.get_commit(oid)?)),
            ObjectType::Snapshot => {
                let s = self.store.get_snapshot(oid)?;
                Ok((s.commit, self.store.get_commit(s.commit)?))
            }
            other => Err(Error::Invalid(format!(
                "{spec} is {}, not a commit",
                other.as_str()
            ))),
        }
    }

    fn resolve_spec_oid(&self, spec: &str) -> Result<ObjectId> {
        match parse_spec(spec)? {
            Spec::Oid(id) => Ok(id),
            Spec::Ref(name) => {
                let r = self
                    .store
                    .meta
                    .get_ref(&name)?
                    .ok_or_else(|| Error::NotFound(format!("ref {name}")))?;
                Ok(r.oid)
            }
        }
    }

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
            } else if Self::ref_unrestricted(cap) {
                self.check(cap, Op::Write, None)?;
            } else {
                return Err(Error::Denied(
                    "ref-scoped caps cannot mount raw oids read-write".into(),
                ));
            }
        } else {
            self.check_spec_read(cap, spec)?;
        }
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
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let rel = rel_of(&m.path, path)?;
        let tree = self.mount_tree(&m.spec)?;
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
        self.require_ns(cap, ns)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        self.check_spec_read(cap, &m.spec)?;
        let ov = self.store.meta.overlay_list(ns, &m.path)?;
        let tree = self.mount_tree(&m.spec)?;
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
        let commit = Commit {
            tree: new_tree,
            parents: vec![pin],
            agent: cap.agent_id().into(),
            msg: msg.into(),
            ts: now_ms(),
            landmark: false,
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

    pub fn branch(&self, cap: &Cap, from: &str, name: &str) -> Result<ObjectId> {
        self.check(cap, Op::Branch, Some(name))?;
        self.check_spec_read(cap, from)?;
        let (oid, _) = self.peel_commit(from)?;
        self.store
            .meta
            .insert_ref(name, oid, "commit", false, false, cap.agent_id(), "branch")?;
        Ok(oid)
    }

    pub fn merge(
        &self,
        cap: &Cap,
        into: &str,
        from: &str,
        resolved: Option<ObjectId>,
    ) -> Result<CasResult> {
        let into_row = self
            .store
            .meta
            .get_ref(into)?
            .ok_or_else(|| Error::NotFound(format!("ref {into}")))?;
        if into_row.protected {
            self.check(cap, Op::Merge, Some(into))?;
        } else {
            self.check(cap, Op::Write, Some(into))?;
        }
        self.check_spec_read(cap, from)?;
        let ours_c = self.store.get_commit(into_row.oid)?;
        let (theirs_oid, theirs_c) = self.peel_commit(from)?;
        let tree = if let Some(t) = resolved {
            if self.store.object_type(t)? != ObjectType::Tree {
                return Err(Error::Invalid("resolved oid is not a tree".into()));
            }
            t
        } else {
            let bases = merge_bases(&self.store, into_row.oid, theirs_oid)?;
            if bases.len() > 1 {
                let base_trees = bases
                    .iter()
                    .map(|id| self.store.get_commit(*id).map(|c| c.tree))
                    .collect::<Result<Vec<_>>>()?;
                let conflict = Conflict {
                    bases: base_trees,
                    ours: ours_c.tree,
                    theirs: theirs_c.tree,
                    paths: vec![],
                    causal: vec![into_row.oid, theirs_oid],
                };
                let oid = self.store.put_conflict(&conflict)?;
                let name = format!("conflicts/{into}/{}", ulid::Ulid::new());
                self.store.meta.insert_ref(
                    &name,
                    oid,
                    "conflict",
                    false,
                    false,
                    cap.agent_id(),
                    "multiple-merge-bases",
                )?;
                self.stats.merge_conflict.fetch_add(1, Ordering::Relaxed);
                return Err(Error::MergeConflict(oid));
            }
            let base_tree = match bases.as_slice() {
                [id] => Some(self.store.get_commit(*id)?.tree),
                [] => None,
                _ => unreachable!("multiple bases handled above"),
            };
            match three_way(&self.store, base_tree, ours_c.tree, theirs_c.tree)? {
                MergeOutcome::Tree(t) => t,
                MergeOutcome::Conflict(mut c) => {
                    c.causal = vec![into_row.oid, theirs_oid];
                    let oid = self.store.put_conflict(&c)?;
                    let name = format!("conflicts/{into}/{}", ulid::Ulid::new());
                    self.store.meta.insert_ref(
                        &name,
                        oid,
                        "conflict",
                        false,
                        false,
                        cap.agent_id(),
                        "conflict",
                    )?;
                    self.stats.merge_conflict.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::MergeConflict(oid));
                }
            }
        };
        let commit = Commit {
            tree,
            parents: vec![into_row.oid, theirs_oid],
            agent: cap.agent_id().into(),
            msg: format!("merge {from} into {into}"),
            ts: now_ms(),
            landmark: false,
        };
        let cid = self.store.put_commit(&commit)?;
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
    }

    pub fn seal(&self, cap: &Cap, r#ref: &str, tag: &str) -> Result<ObjectId> {
        self.check(cap, Op::Seal, Some(r#ref))?;
        self.check(cap, Op::Seal, Some(&format!("tags/{tag}")))?;
        if !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            || tag.is_empty()
            || tag.len() > 64
        {
            return Err(Error::Invalid("bad tag".into()));
        }
        let row = self
            .store
            .meta
            .get_ref(r#ref)?
            .ok_or_else(|| Error::NotFound(r#ref.into()))?;
        let commit = self.store.get_commit(row.oid)?;
        let oids = self.store.reachable_oids(commit.tree)?;
        let mut pairs = Vec::new();
        for id in &oids {
            let agent = self
                .store
                .meta
                .intro_get(*id)?
                .unwrap_or_else(|| "unknown".into());
            let mut k = Vec::new();
            encode_text(&mut k, &id.hex());
            let mut v = Vec::new();
            encode_text(&mut v, &agent);
            pairs.push((k, v));
        }
        let mut map = Vec::new();
        encode_map_sorted(&mut map, pairs);
        let prov = self.store.put_blob_data(&map)?;
        let sk = SigningKey::from_bytes(&self.seal_seed);
        let mut snap = Snapshot {
            tree: commit.tree,
            commit: row.oid,
            tag: tag.to_string(),
            ts: now_ms(),
            prov,
            pk: self.seal_pk,
            sig: [0u8; 64],
        };
        let unsigned = snap.encode_unsigned();
        let h = hash_bytes(&unsigned);
        let sig: Signature = sk.sign(h.as_bytes());
        snap.sig = sig.to_bytes();
        let snap_oid = self.store.put_snapshot(&snap)?;
        self.store
            .meta
            .commit_seal(tag, snap_oid, row.oid, commit.tree, cap.agent_id())?;
        Ok(snap_oid)
    }

    pub fn verify_tag(&self, cap: &Cap, tag: &str) -> Result<ObjectId> {
        let tag_ref_name = format!("tags/{tag}");
        self.check(cap, Op::Read, Some(&tag_ref_name))?;
        let tag_ref = self
            .store
            .meta
            .get_ref(&tag_ref_name)?
            .ok_or_else(|| Error::NotFound(format!("ref {tag_ref_name}")))?;
        let (snap_oid, commit_oid, tree_oid) = self
            .store
            .meta
            .get_seal(tag)?
            .ok_or_else(|| Error::NotFound(format!("tag {tag}")))?;
        if tag_ref.oid != snap_oid
            || tag_ref.kind != "snapshot"
            || !tag_ref.protected
            || !tag_ref.sealed
        {
            return Err(Error::Corrupt("sealed tag ref metadata mismatch".into()));
        }

        let snap = Snapshot::decode(&self.store.get_raw_verified(snap_oid)?)?;
        if snap.pk != self.seal_pk {
            return Err(Error::Corrupt(
                "snapshot key is not this forge's trusted seal key".into(),
            ));
        }
        if snap.tag != tag {
            return Err(Error::Corrupt("snapshot tag mismatch".into()));
        }
        if snap.commit != commit_oid || snap.tree != tree_oid {
            return Err(Error::Corrupt("seal table snapshot mismatch".into()));
        }
        let commit = Commit::decode(&self.store.get_raw_verified(commit_oid)?)?;
        if commit.tree != tree_oid {
            return Err(Error::Corrupt("sealed commit tree mismatch".into()));
        }
        Blob::decode(&self.store.get_raw_verified(snap.prov)?)?;

        let h = hash_bytes(&snap.encode_unsigned());
        let pk =
            VerifyingKey::from_bytes(&self.seal_pk).map_err(|e| Error::Corrupt(e.to_string()))?;
        pk.verify(h.as_bytes(), &Signature::from_bytes(&snap.sig))
            .map_err(|_| Error::Corrupt("snapshot signature".into()))?;
        let walked = self.store.reachable_oids_verified(tree_oid)?;
        if !walked.contains(&tree_oid) {
            return Err(Error::Corrupt("tree walk".into()));
        }
        Ok(snap_oid)
    }

    pub fn export_tar(&self, cap: &Cap, spec: &str, out: &Path) -> Result<()> {
        self.check_spec_read(cap, spec)?;
        crate::export::export_tar(&self.store, self.resolve_tree(spec)?, out)
    }

    fn resolve_tree(&self, spec: &str) -> Result<ObjectId> {
        if let Ok((.., c)) = self.peel_commit(spec) {
            return Ok(c.tree);
        }
        let oid = self.resolve_spec_oid(spec)?;
        match self.store.object_type(oid)? {
            ObjectType::Tree => Ok(oid),
            ObjectType::Snapshot => Ok(self.store.get_snapshot(oid)?.tree),
            _ => Err(Error::Invalid("cannot export".into())),
        }
    }

    pub fn import_dir(&self, cap: &Cap, dir: &Path, r#ref: &str) -> Result<ObjectId> {
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

    pub fn landmark(&self, cap: &Cap, oid: ObjectId) -> Result<()> {
        self.check(cap, Op::Write, None)?;
        self.store.meta.landmark(oid, "commit", "explicit")?;
        Ok(())
    }

    pub fn log(&self, cap: &Cap, r#ref: &str, n: usize) -> Result<Vec<(ObjectId, String, String)>> {
        self.check(cap, Op::Read, Some(r#ref))?;
        let rows = self.store.meta.reflog(r#ref, n)?;
        Ok(rows
            .into_iter()
            .map(|(_o, new, agent, reason)| (new, agent, reason))
            .collect())
    }

    pub fn show(&self, cap: &Cap, spec: &str) -> Result<String> {
        self.check_spec_read(cap, spec)?;
        let oid = self.resolve_spec_oid(spec)?;
        let ty = self.store.object_type(oid)?;
        if ty == ObjectType::Conflict {
            let conflict = self.store.get_conflict(oid)?;
            let fmt_oid = |id: Option<ObjectId>| id.map(|v| v.hex()).unwrap_or_else(|| "-".into());
            let mut out = String::new();
            out.push_str(&format!("conflict {oid}\n"));
            out.push_str(&format!(
                "bases {}\n",
                if conflict.bases.is_empty() {
                    "-".into()
                } else {
                    conflict
                        .bases
                        .iter()
                        .map(ObjectId::hex)
                        .collect::<Vec<_>>()
                        .join(",")
                }
            ));
            out.push_str(&format!("ours {}\n", conflict.ours));
            out.push_str(&format!("theirs {}\n", conflict.theirs));
            for path in conflict.paths {
                out.push_str(&format!(
                    "path {} a={} b={} base={}\n",
                    path.path,
                    fmt_oid(path.a),
                    fmt_oid(path.b),
                    fmt_oid(path.base)
                ));
            }
            if !conflict.causal.is_empty() {
                out.push_str(&format!(
                    "causal {}\n",
                    conflict
                        .causal
                        .iter()
                        .map(ObjectId::hex)
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            return Ok(out.trim_end().to_string());
        }
        let bytes = self.store.get_raw(oid)?;
        Ok(format!("{} {} bytes", ty.as_str(), bytes.len()))
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

fn import_walk(store: &Store, dir: &Path, source_root: bool) -> Result<ObjectId> {
    let mut entries = Vec::new();
    // Never turn a per-entry enumeration error into a successful partial import.
    let mut kids: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    kids.sort_by_key(|e| e.file_name());
    for k in kids {
        let name = k
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        // Root control directories are outside the import domain. Nested names
        // with the same spelling are ordinary user data and must be preserved.
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_symlink() {
            return Err(Error::Invalid(format!(
                "import refuses symlink {}",
                k.path().display()
            )));
        }
        if ft.is_dir() {
            let id = import_walk(store, &k.path(), false)?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "import refuses unsupported file type {}",
                k.path().display()
            )));
        } else {
            let data = fs::read(k.path())?;
            let id = store.put_blob_data(&data)?;
            let exec = is_exec(&k.path())?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Blob,
                id,
                exec,
            });
        }
    }
    store.put_tree(&Tree::new(entries)?)
}

fn is_exec(p: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(p)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(false)
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid(format!("path contains NUL: {}", path.display())))
}

fn publish_noreplace(from: &Path, to: &Path) -> Result<()> {
    let from_c = path_cstring(from)?;
    let to_c = path_cstring(to)?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };

    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renamex_np(
            from_c.as_ptr(),
            to_c.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(Error::Invalid(format!("already a forge: {}", to.display())));
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if error.raw_os_error() == Some(libc::EINVAL) {
            return Err(Error::Invalid(
                "filesystem does not support atomic no-replace repository publication".into(),
            ));
        }
        Err(error.into())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = (from_c, to_c);
        Err(Error::Invalid(
            "atomic no-replace repository publication is unsupported on this platform".into(),
        ))
    }
}

fn acquire_cell_lock(root: &Path, exclusive: bool) -> Result<File> {
    let path = root.join("LOCK");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path)?;

    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() || metadata.nlink() != 1 {
            return Err(Error::Denied(format!(
                "LOCK must be a single-link regular file: {}",
                path.display()
            )));
        }
    }

    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Busy(if exclusive {
            "forge cell is in use by a direct client or daemon".into()
        } else {
            "forge daemon owns this cell; use the socket or stop forge serve".into()
        })),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn validate_repo_version(root: &Path) -> Result<()> {
    let version = fs::read(root.join("VERSION"))?;
    match version.as_slice() {
        b"1" | b"1\n" | b"1\r\n" => Ok(()),
        _ => Err(Error::Invalid(format!(
            "unsupported ForgeFS repository VERSION at {} (this binary supports VERSION 1)",
            root.display()
        ))),
    }
}

fn forge_root(dir: &Path) -> PathBuf {
    if dir.ends_with(".forge") {
        dir.to_path_buf()
    } else {
        dir.join(".forge")
    }
}

pub fn find_forge(start: &Path) -> Result<PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    loop {
        let cand = if cur.ends_with(".forge") {
            cur.clone()
        } else {
            cur.join(".forge")
        };
        if cand.join("VERSION").exists() {
            validate_repo_version(&cand)?;
            return Ok(cand);
        }
        if !cur.pop() {
            break;
        }
    }
    Err(Error::NotFound(format!(
        "no .forge above {}",
        start.display()
    )))
}

fn secure_key_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_key_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(Error::Denied(format!(
                "key directory {} must be mode 0700, got {mode:04o}",
                path.display()
            )));
        }
        for name in ["root.secret", "seal.ed25519", "root.cap", "integrator.cap"] {
            let secret = path.join(name);
            let mode = fs::metadata(&secret)?.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(Error::Denied(format!(
                    "secret {} must be mode 0600, got {mode:04o}",
                    secret.display()
                )));
            }
        }
    }
    Ok(())
}

fn write_secret(path: PathBuf, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        opts.open(&path)?
    };
    #[cfg(not(unix))]
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn write_public(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn read32(path: &Path) -> Result<[u8; 32]> {
    let v = fs::read(path)?;
    if v.len() != 32 {
        return Err(Error::Corrupt(format!(
            "expected 32 bytes in {}",
            path.display()
        )));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Forge, Cap) {
        let d = tempdir().unwrap();
        let f = Forge::init(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        (d, f, cap)
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn publish_never_replaces_an_existing_empty_root() {
        let d = tempdir().unwrap();
        let staging = d.path().join("staging");
        let root = d.path().join(".forge");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(staging.join("VERSION"), b"1\n").unwrap();

        assert!(publish_noreplace(&staging, &root).is_err());
        assert!(root.is_dir());
        assert!(staging.join("VERSION").exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn staged_shared_lock_remains_authoritative_after_publish() {
        let d = tempdir().unwrap();
        let staging = d.path().join("staging");
        let root = d.path().join(".forge");
        fs::create_dir(&staging).unwrap();
        let shared = acquire_cell_lock(&staging, false).unwrap();
        publish_noreplace(&staging, &root).unwrap();

        assert!(matches!(
            acquire_cell_lock(&root, true),
            Err(Error::Busy(_))
        ));
        drop(shared);
        drop(acquire_cell_lock(&root, true).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let d = tempdir().unwrap();
        let root = d.path().join(".forge");
        fs::create_dir(&root).unwrap();
        let victim = d.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, root.join("LOCK")).unwrap();

        assert!(acquire_cell_lock(&root, true).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }

    #[test]
    fn init_and_session_write_checkin() {
        let (_d, f, cap) = setup();
        let ns = f.session_open(&cap, "main").unwrap();
        f.write(&cap, &ns, "/hello.txt", b"hi", false).unwrap();
        let r = f.checkin(&cap, &ns, "/", "add hello").unwrap();
        assert!(matches!(r, CasResult::Updated { .. }));
        let data = f.read(&cap, &ns, "/hello.txt").unwrap();
        assert_eq!(data, b"hi");
    }

    #[test]
    fn parallel_private_namespaces() {
        let d = tempdir().unwrap();
        let f = Arc::new(Forge::init(d.path()).unwrap());
        let cap = Arc::new(f.root_cap().unwrap());
        let mut hs = vec![];
        for i in 0..32 {
            let f = f.clone();
            let cap = cap.clone();
            hs.push(thread::spawn(move || {
                let agent = f
                    .grant(
                        &cap,
                        vec![
                            format!("ops=read,write,branch"),
                            format!("agent=w{i}"),
                            // One ref set: OR of private heads and the snapshot of main.
                            // Two separate `ref=` caveats would AND and deny `session_open` from main.
                            format!("ref=heads/agents/*,main"),
                        ],
                    )
                    .unwrap();
                let ns = f.session_open(&agent, "main").unwrap();
                let p = format!("/w{i}.txt");
                f.write(&agent, &ns, &p, format!("{i}").as_bytes(), false)
                    .unwrap();
                f.checkin(&agent, &ns, "/", "w").unwrap()
            }));
        }
        let mut updated = 0;
        for h in hs {
            match h.join().unwrap() {
                CasResult::Updated { .. } => updated += 1,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(updated, 32);
    }

    #[test]
    fn shared_ref_forks() {
        let (_d, f, cap) = setup();
        f.branch(&cap, "main", "shared").unwrap();
        let n1 = f.session_open(&cap, "shared").unwrap();
        let n2 = f.session_open(&cap, "shared").unwrap();
        f.mount(&cap, &n1, "/", "ref:shared", true).unwrap();
        f.mount(&cap, &n2, "/", "ref:shared", true).unwrap();
        f.write(&cap, &n1, "/a.txt", b"a", false).unwrap();
        f.write(&cap, &n2, "/b.txt", b"b", false).unwrap();
        let f = std::sync::Arc::new(f);
        let cap = std::sync::Arc::new(cap);
        let n1c = n1.clone();
        let n2c = n2.clone();
        let f1 = f.clone();
        let f2 = f.clone();
        let c1 = cap.clone();
        let c2 = cap.clone();
        let h1 = std::thread::spawn(move || f1.checkin(&c1, &n1c, "/", "a"));
        let h2 = std::thread::spawn(move || f2.checkin(&c2, &n2c, "/", "b"));
        let r1 = h1.join().unwrap().unwrap();
        let r2 = h2.join().unwrap().unwrap();
        let forks = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, CasResult::Forked { .. }))
            .count();
        let ups = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, CasResult::Updated { .. }))
            .count();
        // Concurrent CAS: 1 update + 1 fork. If the scheduler serializes
        // the two checkins, disjoint overlays compose into 2 updates. Both
        // are correct; lost updates are not.
        assert_eq!(ups + forks, 2, "{r1:?} {r2:?}");
        assert!(ups >= 1, "{r1:?} {r2:?}");
    }

    #[test]
    fn agent_cannot_seal_main() {
        let (_d, f, cap) = setup();
        let agent = f
            .grant(
                &cap,
                vec!["ops=read,write,branch".into(), "ref!=main".into()],
            )
            .unwrap();
        assert!(f.seal(&agent, "main", "v1.0").is_err());
        let integ = f.integrator_cap().unwrap();
        let oid = f.seal(&integ, "main", "v1.0").unwrap();
        f.verify_tag(&cap, "v1.0").unwrap();
        assert_ne!(oid, ObjectId::ZERO);
    }

    #[test]
    fn overlay_survives_reopen() {
        let d = tempdir().unwrap();
        let ns;
        {
            let f = Forge::init(d.path()).unwrap();
            let cap = f.root_cap().unwrap();
            ns = f.session_open(&cap, "main").unwrap();
            f.write(&cap, &ns, "/x.txt", b"stay", false).unwrap();
        }
        let f = Forge::open(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        assert_eq!(f.read(&cap, &ns, "/x.txt").unwrap(), b"stay");
        f.checkin(&cap, &ns, "/", "later").unwrap();
    }

    #[test]
    fn merge_seal_export_verify() {
        let d = tempdir().unwrap();
        let f = Forge::init(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        let integ = f.integrator_cap().unwrap();
        let ns = f.session_open(&cap, "main").unwrap();
        f.write(&cap, &ns, "/paper.txt", b"final", false).unwrap();
        let CasResult::Updated { name, .. } = f.checkin(&cap, &ns, "/", "paper").unwrap() else {
            panic!("expected update");
        };
        f.merge(&integ, "main", &name, None).unwrap();
        f.seal(&integ, "main", "v1.0").unwrap();
        f.verify_tag(&cap, "v1.0").unwrap();
        let tar1 = d.path().join("a.tar");
        let tar2 = d.path().join("b.tar");
        f.export_tar(&cap, "tags/v1.0", &tar1).unwrap();
        f.export_tar(&cap, "tags/v1.0", &tar2).unwrap();
        assert_eq!(std::fs::read(&tar1).unwrap(), std::fs::read(&tar2).unwrap());
        let bytes = std::fs::read(&tar1).unwrap();
        assert!(bytes.len() > 64);
    }

    #[test]
    fn disjoint_merge_then_same_path_conflict() {
        let d = tempdir().unwrap();
        let f = Forge::init(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        let n1 = f.session_open(&cap, "main").unwrap();
        let n2 = f.session_open(&cap, "main").unwrap();
        f.write(&cap, &n1, "/a.txt", b"a", false).unwrap();
        f.write(&cap, &n2, "/b.txt", b"b", false).unwrap();
        let CasResult::Updated { name: r1, .. } = f.checkin(&cap, &n1, "/", "a").unwrap() else {
            panic!();
        };
        let CasResult::Updated { name: r2, .. } = f.checkin(&cap, &n2, "/", "b").unwrap() else {
            panic!();
        };
        f.merge(&cap, &r1, &r2, None).unwrap();
        // same-path conflict
        let n3 = f.session_open(&cap, "main").unwrap();
        let n4 = f.session_open(&cap, "main").unwrap();
        f.write(&cap, &n3, "/c.txt", b"1", false).unwrap();
        f.write(&cap, &n4, "/c.txt", b"2", false).unwrap();
        let CasResult::Updated { name: r3, .. } = f.checkin(&cap, &n3, "/", "c1").unwrap() else {
            panic!();
        };
        let CasResult::Updated { name: r4, .. } = f.checkin(&cap, &n4, "/", "c2").unwrap() else {
            panic!();
        };
        let err = f.merge(&cap, &r3, &r4, None).unwrap_err();
        assert!(matches!(err, Error::MergeConflict(_)));
    }

    #[test]
    fn agent_cannot_checkin_protected_main() {
        let (_d, f, cap) = setup();
        let agent = f
            .grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    "agent=mallory".into(),
                    "ref=main,heads/agents/*".into(),
                ],
            )
            .unwrap();
        let ns = f.session_open(&agent, "main").unwrap();
        f.mount(&agent, &ns, "/", "ref:main", true).unwrap();
        f.write(&agent, &ns, "/evil.txt", b"nope", false).unwrap();
        let err = f.checkin(&agent, &ns, "/", "clobber main").unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
    }

    #[test]
    fn agents_cannot_read_each_others_namespaces() {
        let (_d, f, cap) = setup();
        let alice = f
            .grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    "agent=alice".into(),
                    "ref=heads/agents/alice/*,main".into(),
                ],
            )
            .unwrap();
        let bob = f
            .grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    "agent=bob".into(),
                    "ref=heads/agents/bob/*,main".into(),
                ],
            )
            .unwrap();
        let a_ns = f.session_open(&alice, "main").unwrap();
        let _b_ns = f.session_open(&bob, "main").unwrap();
        f.write(&alice, &a_ns, "/secret.txt", b"alice", false)
            .unwrap();
        let err = f.read(&bob, &a_ns, "/secret.txt").unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
    }

    #[test]
    fn tampered_object_fails_verify() {
        let d = tempdir().unwrap();
        let f = Forge::init(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        let integ = f.integrator_cap().unwrap();
        let ns = f.session_open(&cap, "main").unwrap();
        f.write(&cap, &ns, "/paper.txt", b"final", false).unwrap();
        let CasResult::Updated { name, .. } = f.checkin(&cap, &ns, "/", "paper").unwrap() else {
            panic!("expected update");
        };
        f.merge(&integ, "main", &name, None).unwrap();
        f.seal(&integ, "main", "v1.0").unwrap();
        f.verify_tag(&cap, "v1.0").unwrap();
        let obj_root = f.root().join("objects");
        drop(f);
        let mut stack = vec![obj_root];
        while let Some(p) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        let _ = std::fs::write(&p, b"tamper");
                    }
                }
            }
        }
        let f = Forge::open(d.path()).unwrap();
        let cap = f.root_cap().unwrap();
        assert!(f.verify_tag(&cap, "v1.0").is_err());
    }

    #[test]
    fn stale_observation_of_main_is_detected() {
        let (_d, f, cap) = setup();
        let integ = f.integrator_cap().unwrap();
        let alice = f
            .grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    "agent=alice".into(),
                    "ref=heads/agents/alice/*,main".into(),
                ],
            )
            .unwrap();
        let bob = f
            .grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    "agent=bob".into(),
                    "ref=heads/agents/bob/*,main".into(),
                ],
            )
            .unwrap();
        let a = f.session_open(&alice, "main").unwrap();
        f.write(&alice, &a, "/x.txt", b"v1", false).unwrap();
        let CasResult::Updated { name: aref, .. } = f.checkin(&alice, &a, "/", "x").unwrap() else {
            panic!("alice checkin");
        };
        f.merge(&integ, "main", &aref, None).unwrap();

        let b = f.session_open(&bob, "main").unwrap();
        assert_eq!(f.read(&bob, &b, "/main/x.txt").unwrap(), b"v1");

        let a2 = f.session_open(&alice, "main").unwrap();
        f.write(&alice, &a2, "/x.txt", b"v2", false).unwrap();
        let CasResult::Updated { name: aref2, .. } = f.checkin(&alice, &a2, "/", "x2").unwrap()
        else {
            panic!("alice2");
        };
        f.merge(&integ, "main", &aref2, None).unwrap();

        f.write(&bob, &b, "/y.txt", b"unrelated", false).unwrap();
        let err = f.checkin(&bob, &b, "/", "y").unwrap_err();
        assert!(
            matches!(err, Error::StaleObservation { .. }),
            "expected stale observation, got {err:?}"
        );
    }
}
