//! The native Forge API. Agents speak this; POSIX is an adapter.

mod export;
mod serve;

pub use serve::serve;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use forge_cap::{attenuate, mint_integrator, mint_root, verify, Cap, Op};
use forge_core::cbor::{encode_map_sorted, encode_text};
use forge_core::tree::apply_overlay;
use forge_core::{hash_bytes, now_ms, Commit, Snapshot, Tree};
use forge_merge::{lca, three_way, MergeOutcome};
use forge_ns::{
    longest_mount, ls as ns_ls, normalize_abs, overlay_map, parse_spec, rel_of, resolve, Mode,
    Mount, Resolved, Spec,
};
use forge_store::{sanitize_agent, Store};
use forge_types::{CasResult, Error, ObjectId, ObjectType, RefRow, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Forge {
    pub store: Store,
    hmac_key: [u8; 32],
    seal_seed: [u8; 32],
    seal_pk: [u8; 32],
    root: PathBuf,
}

impl Forge {
    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        if root.join("VERSION").exists() {
            return Err(Error::Invalid(format!("already a forge: {}", root.display())));
        }
        fs::create_dir_all(root.join("keys"))?;
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("tmp"))?;
        fs::write(root.join("VERSION"), b"1\n")?;
        fs::write(
            root.join("config.toml"),
            b"[store]\nhash = \"blake3-256\"\nblob_warn_bytes = 67108864\n\n[serve]\nlisten = \"127.0.0.1:4077\"\n",
        )?;

        let mut hmac_key = [0u8; 32];
        let mut seal_seed = [0u8; 32];
        getrandom::getrandom(&mut hmac_key).map_err(|e| Error::Internal(e.to_string()))?;
        getrandom::getrandom(&mut seal_seed).map_err(|e| Error::Internal(e.to_string()))?;
        let sk = SigningKey::from_bytes(&seal_seed);
        let pk = sk.verifying_key().to_bytes();

        write_secret(root.join("keys/root.secret"), &hmac_key)?;
        write_secret(root.join("keys/seal.ed25519"), &seal_seed)?;
        fs::write(root.join("keys/seal.pub"), pk)?;

        let store = Store::open(&root)?;
        store.meta.set_cap_root(&hmac_key, &pk)?;

        let root_cap = mint_root(&hmac_key)?;
        let integ = mint_integrator(&hmac_key)?;
        fs::write(root.join("keys/root.cap"), root_cap.to_token())?;
        fs::write(root.join("keys/integrator.cap"), integ.to_token())?;

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
        store.record_intros(None, empty, cid, "init")?;
        store
            .meta
            .insert_ref("main", cid, "commit", true, false, "init", "init")?;
        store.meta.landmark(cid, "commit", "init")?;

        Ok(Self {
            store,
            hmac_key,
            seal_seed,
            seal_pk: pk,
            root,
        })
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let root = find_forge(dir)?;
        if root.join("LOCK").exists() && root.join("forge.sock").exists() {
            return Err(Error::Busy(
                "daemon holds LOCK; speak the socket or stop forge serve".into(),
            ));
        }
        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let store = Store::open(&root)?;
        let sk = SigningKey::from_bytes(&seal_seed);
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk: sk.verifying_key().to_bytes(),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
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

    pub fn grant(&self, cap: &Cap, extra: Vec<String>) -> Result<Cap> {
        self.check(cap, Op::Grant, None)?;
        attenuate(&self.hmac_key, cap, extra)
    }

    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        self.check(cap, Op::Read, None)?;
        self.store.meta.list_refs()
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
        self.check(cap, Op::Read, Some(from))?;
        let (cid, commit) = self.peel_commit(from)?;
        let ns_id = ulid::Ulid::new().to_string();
        let agent = sanitize_agent(cap.agent_id());
        let live = format!("heads/agents/{agent}/{ns_id}");
        self.store.meta.insert_namespace(&ns_id, cap.agent_id())?;
        self.store.meta.insert_ref(
            &live,
            cid,
            "commit",
            false,
            false,
            cap.agent_id(),
            "session",
        )?;
        self.store
            .meta
            .insert_mount(&ns_id, "/", &format!("ref:{live}"), "rw")?;
        self.store
            .meta
            .insert_mount(&ns_id, "/main", "ref:main", "ro")?;
        let _ = commit;
        Ok(ns_id)
    }

    pub fn mount(&self, cap: &Cap, ns: &str, path: &str, spec: &str, rw: bool) -> Result<()> {
        let path = normalize_abs(path)?;
        let mode = if rw { "rw" } else { "ro" };
        if rw {
            if let Spec::Ref(n) = parse_spec(spec)? {
                self.check(cap, Op::Write, Some(&n))?;
            } else {
                self.check(cap, Op::Write, None)?;
            }
        } else {
            self.check(cap, Op::Read, None)?;
        }
        self.store.meta.get_namespace(ns)?;
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
            other => Err(Error::Invalid(format!(
                "cannot mount {}",
                other.as_str()
            ))),
        }
    }

    pub fn ls(&self, cap: &Cap, ns: &str, path: &str) -> Result<Vec<(String, String, String, bool)>> {
        self.check(cap, Op::Read, None)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
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
        self.check(cap, Op::Read, None)?;
        let mounts = self.mounts(ns)?;
        let m = longest_mount(&mounts, path)?;
        let ov = self.store.meta.overlay_list(ns, &m.path)?;
        let tree = self.mount_tree(&m.spec)?;
        match resolve(&self.store, &mounts, &ov, tree, path)? {
            Resolved::Blob { id, .. } => self.store.get_blob_data(id),
            Resolved::Tree(_) => Err(Error::Invalid("read of directory".into())),
        }
    }

    pub fn write(&self, cap: &Cap, ns: &str, path: &str, data: &[u8], exec: bool) -> Result<ObjectId> {
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
        self.store.meta.overlay_upsert(ns, &m.path, &rel, None, false)?;
        Ok(())
    }

    pub fn checkin(&self, cap: &Cap, ns: &str, mount: &str, msg: &str) -> Result<CasResult> {
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
        let row = self
            .store
            .meta
            .get_ref(&ref_name)?
            .ok_or_else(|| Error::NotFound(ref_name.clone()))?;
        let base_commit = self.store.get_commit(row.oid)?;
        let ov_rows = self.store.meta.overlay_list(ns, &m.path)?;
        let ov = overlay_map(&ov_rows);
        let new_tree = apply_overlay(Some(base_commit.tree), &ov, &self.store)?;
        if new_tree == base_commit.tree {
            return Ok(CasResult::Noop {
                name: ref_name,
                oid: row.oid,
            });
        }
        let commit = Commit {
            tree: new_tree,
            parents: vec![row.oid],
            agent: cap.agent_id().into(),
            msg: msg.into(),
            ts: now_ms(),
            landmark: false,
        };
        let cid = self.store.put_commit(&commit)?;
        self.store
            .record_intros(Some(base_commit.tree), new_tree, cid, cap.agent_id())?;
        let result = self.store.meta.cas_ref(
            &ref_name,
            row.oid,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
        )?;
        match &result {
            CasResult::Updated { .. } => {
                self.store.meta.overlay_clear(ns, &m.path)?;
            }
            CasResult::Forked { fork, .. } => {
                self.store
                    .meta
                    .update_mount_spec(ns, &m.path, &format!("ref:{fork}"))?;
            }
            CasResult::Noop { .. } => {}
        }
        Ok(result)
    }

    pub fn branch(&self, cap: &Cap, from: &str, name: &str) -> Result<ObjectId> {
        self.check(cap, Op::Branch, Some(name))?;
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
        let ours_c = self.store.get_commit(into_row.oid)?;
        let (theirs_oid, theirs_c) = self.peel_commit(from)?;
        let tree = if let Some(t) = resolved {
            t
        } else {
            let base = lca(&self.store, into_row.oid, theirs_oid)?;
            let base_tree = match base {
                Some(id) => self.store.get_commit(id).ok().map(|c| c.tree),
                None => None,
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
        self.store
            .record_intros(Some(ours_c.tree), tree, cid, cap.agent_id())?;
        self.store.meta.cas_ref(
            into,
            into_row.oid,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
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
        let tag_ref = format!("tags/{tag}");
        self.store.meta.insert_ref(
            &tag_ref,
            snap_oid,
            "snapshot",
            true,
            true,
            cap.agent_id(),
            "seal",
        )?;
        self.store
            .meta
            .insert_seal(tag, snap_oid, row.oid, commit.tree)?;
        self.store.meta.landmark(snap_oid, "snapshot", "seal")?;
        self.store.meta.landmark(row.oid, "commit", "seal")?;
        Ok(snap_oid)
    }

    pub fn verify_tag(&self, cap: &Cap, tag: &str) -> Result<ObjectId> {
        self.check(cap, Op::Read, Some(&format!("tags/{tag}")))?;
        let (snap_oid, _c, tree) = self
            .store
            .meta
            .get_seal(tag)?
            .ok_or_else(|| Error::NotFound(format!("tag {tag}")))?;
        let snap = self.store.get_snapshot(snap_oid)?;
        if snap.tree != tree {
            return Err(Error::Corrupt("seal table tree mismatch".into()));
        }
        let unsigned = snap.encode_unsigned();
        let h = hash_bytes(&unsigned);
        let pk = VerifyingKey::from_bytes(&snap.pk)
            .map_err(|e| Error::Corrupt(e.to_string()))?;
        let sig = Signature::from_bytes(&snap.sig);
        pk.verify(h.as_bytes(), &sig)
            .map_err(|_| Error::Corrupt("snapshot signature".into()))?;
        let walked = self.store.reachable_oids(snap.tree)?;
        if walked.is_empty() || walked[0] != snap.tree {
            return Err(Error::Corrupt("tree walk".into()));
        }
        // re-hash every object file
        for id in walked {
            let _ = self.store.get_raw(id)?;
        }
        Ok(snap_oid)
    }

    pub fn export_tar(&self, cap: &Cap, spec: &str, out: &Path) -> Result<()> {
        self.check(cap, Op::Read, None)?;
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
        let tree = import_walk(&self.store, dir)?;
        let commit = Commit {
            tree,
            parents: vec![],
            agent: cap.agent_id().into(),
            msg: format!("import {}", dir.display()),
            ts: now_ms(),
            landmark: false,
        };
        let cid = self.store.put_commit(&commit)?;
        self.store.record_intros(None, tree, cid, cap.agent_id())?;
        match self.store.meta.get_ref(r#ref)? {
            Some(row) => {
                self.store.meta.cas_ref(
                    r#ref,
                    row.oid,
                    cid,
                    "commit",
                    cap.agent_id(),
                    cap.agent_id(),
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
        self.check(cap, Op::Read, None)?;
        let oid = if spec.len() == 64 {
            ObjectId::from_hex(spec).unwrap_or(self.resolve_spec_oid(spec)?)
        } else {
            self.resolve_spec_oid(spec)?
        };
        let bytes = self.store.get_raw(oid)?;
        Ok(format!("{} {} bytes", self.store.object_type(oid)?.as_str(), bytes.len()))
    }
}

fn import_walk(store: &Store, dir: &Path) -> Result<ObjectId> {
    let mut entries = Vec::new();
    let mut kids: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    kids.sort_by_key(|e| e.file_name());
    for k in kids {
        let name = k.file_name().to_string_lossy().into_owned();
        if name == ".forge" || name == ".git" {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_dir() {
            let id = import_walk(store, &k.path())?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if ft.is_file() {
            let data = fs::read(k.path())?;
            let id = store.put_blob_data(&data)?;
            let exec = is_exec(&k.path());
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

fn is_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        false
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

fn write_secret(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read32(path: &Path) -> Result<[u8; 32]> {
    let v = fs::read(path)?;
    if v.len() != 32 {
        return Err(Error::Corrupt(format!("expected 32 bytes in {}", path.display())));
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
                            format!("ref=heads/agents/*"),
                            format!("ref=main"),
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
}
