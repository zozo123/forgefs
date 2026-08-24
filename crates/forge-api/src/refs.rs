//! Typed refs, inbox handoffs, history, and object/ref resolution.

use super::*;

impl Forge {
    pub fn refs(&self, cap: &Cap) -> Result<Vec<RefRow>> {
        Ok(self.refs_with_suppressed(cap)?.0)
    }

    /// Enumerate refs visible to `cap` and return how many durable rows were
    /// hidden by ref authority. Names and contents remain undisclosed, but
    /// automation can distinguish an actually complete view from a filtered one.
    pub fn refs_with_suppressed(&self, cap: &Cap) -> Result<(Vec<RefRow>, usize)> {
        self.check(cap, Op::Read, None)?;
        let now = now_ms();
        let mut out = Vec::new();
        let mut suppressed = 0usize;
        for r in self.store.meta.list_refs()? {
            if cap.allows(Op::Read, Some(&r.name), now).is_ok() {
                out.push(r);
            } else {
                suppressed += 1;
            }
        }
        Ok((out, suppressed))
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

    pub(crate) fn resolve_spec_oid(&self, spec: &str) -> Result<ObjectId> {
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

    pub fn branch(&self, cap: &Cap, from: &str, name: &str) -> Result<ObjectId> {
        self.check(cap, Op::Branch, Some(name))?;
        self.check_spec_read(cap, from)?;
        let (oid, _) = self.peel_commit(from)?;
        self.store
            .meta
            .insert_ref(name, oid, "commit", false, false, cap.agent_id(), "branch")?;
        Ok(oid)
    }

    pub fn landmark(&self, cap: &Cap, oid: ObjectId) -> Result<()> {
        // Every other raw-OID entry point in this crate pairs check(.., None)
        // with this guard (check_spec_read, mount --rw oid:, fsck). landmark was
        // the only one that did not, so a cap that could read nothing and move
        // no ref could still write repository metadata.
        if !cap.has_unrestricted_ref_scope() {
            return Err(Error::Denied(
                "ref-scoped caps cannot address raw object ids".into(),
            ));
        }
        self.check(cap, Op::Write, None)?;
        // Record what the object actually is, and refuse one that is not there.
        // A landmark is a GC root, so a dangling or mistyped row is a latent
        // collection hazard that fsck does not currently surface.
        let kind = match self.store.object_type(oid)? {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Commit => "commit",
            ObjectType::Conflict => "conflict",
            ObjectType::Snapshot => "snapshot",
            ObjectType::Contribution => "contribution",
        };
        self.store.meta.landmark(oid, kind, "explicit")?;
        Ok(())
    }

    pub fn log(&self, cap: &Cap, r#ref: &str, n: usize) -> Result<Vec<(ObjectId, String, String)>> {
        self.check(cap, Op::Read, Some(r#ref))?;
        // Exiting 0 with no output made "this ref has no history" and "this ref
        // does not exist" indistinguishable to a caller.
        if self.store.meta.get_ref(r#ref)?.is_none() {
            return Err(Error::NotFound(format!("ref {ref_name}", ref_name = r#ref)));
        }
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
        if ty == ObjectType::Contribution {
            let contribution = self.store.get_contribution(oid)?;
            let mut out = String::new();
            out.push_str(&format!("contribution {oid}\n"));
            out.push_str(&format!("agent {}\n", contribution.agent));
            out.push_str(&format!("base {}\n", contribution.base));
            out.push_str(&format!("tree {}\n", contribution.tree));
            out.push_str(&format!(
                "parents {}\n",
                contribution
                    .parents
                    .iter()
                    .map(ObjectId::hex)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            for read in contribution.reads {
                out.push_str(&format!("read {} {}\n", read.id, read.path));
            }
            for path in contribution.writes {
                out.push_str(&format!("write {path}\n"));
            }
            return Ok(out.trim_end().to_string());
        }
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
