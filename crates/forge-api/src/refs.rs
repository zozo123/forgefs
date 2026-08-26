//! Typed refs, inbox handoffs, history, and object/ref resolution.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::object::decode_object_type;
use forge_core::{now_ms, Commit, Contribution};
use forge_ns::{parse_spec, Spec};
use forge_store::sanitize_agent;
use forge_types::{CasResult, Error, ObjectId, ObjectType, RefRow, Result};

/// A verified ContributionReceipt: what one checkin contributed, and the
/// evidence that everything it names is really there (#71, I25).
///
/// Constructed only by [`Forge::receipt`], which refuses rather than returning
/// a receipt whose edges do not check out, so holding one of these IS the
/// proof. Deterministic fields only; `ts` is advisory metadata and never
/// causal order (I12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The Contribution (`0x06`) object itself.
    pub receipt: ObjectId,
    /// The commit that names this receipt, when the receipt was reached
    /// through one. `None` when the caller named the receipt object directly,
    /// because a receipt does not record who published it.
    pub result: Option<ObjectId>,
    pub agent: String,
    pub base: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    /// The observed frontier: `(path, blob oid)` for every blob the work read.
    /// A directory read and a recorded absence are NOT here -- VERSION 1
    /// `reads` cannot express either -- so this is a subset of what I9
    /// recorded, never the whole observation set.
    pub reads: Vec<(String, ObjectId)>,
    /// Paths the contribution wrote, as a flat list. A frozen VERSION 1
    /// `Contribution` has no add/delete/move tag, so a deletion and a creation
    /// are reported identically here (see `rename_characterisation.rs`).
    pub writes: Vec<String>,
    /// Advisory wall clock. Never an ordering (I12).
    pub ts: u64,
}

impl Receipt {
    /// One line per fact, stable enough for a script to key on.
    pub fn render(&self) -> String {
        let mut out = format!("receipt {}\n", self.receipt);
        if let Some(result) = self.result {
            out.push_str(&format!("result {result}\n"));
        }
        out.push_str(&format!("agent {}\n", self.agent));
        out.push_str(&format!("base {}\n", self.base));
        out.push_str(&format!("tree {}\n", self.tree));
        out.push_str(&format!(
            "parents {}\n",
            if self.parents.is_empty() {
                "-".to_string()
            } else {
                self.parents
                    .iter()
                    .map(ObjectId::hex)
                    .collect::<Vec<_>>()
                    .join(",")
            }
        ));
        for (path, id) in &self.reads {
            out.push_str(&format!("read {id} {path}\n"));
        }
        for path in &self.writes {
            out.push_str(&format!("write {path}\n"));
        }
        out.push_str(&format!("verified {} edges\n", self.verified_edges()));
        out.trim_end().to_string()
    }

    /// How many objects `Forge::receipt` reread and type-checked to produce
    /// this. Reported so "verified" is a count and not an adjective.
    pub fn verified_edges(&self) -> usize {
        // the receipt itself, base, tree, every parent, every read
        3 + self.parents.len() + self.reads.len()
    }
}

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

    /// One verified ContributionReceipt (#71, I25).
    ///
    /// `spec` may name the receipt object itself, or a commit -- or a ref or
    /// sealed tag peeling to one -- in which case the commit's `contrib` edge
    /// is followed and the commit is required to AGREE with what it points at.
    ///
    /// Every object the receipt names is reread from durable bytes and
    /// rehashed before it is reported, so a receipt that claims a result
    /// commit, a base, a tree or an observation that is absent, corrupt, or of
    /// the wrong type is refused -- exit 2 -- rather than rendered. That is the
    /// difference between this and `show`, which renders a Contribution's
    /// fields without checking that any of them are there.
    pub fn receipt(&self, cap: &Cap, spec: &str) -> Result<Receipt> {
        self.check_spec_read(cap, spec)?;
        let named = self.resolve_spec_oid(spec)?;
        // The object the CALLER named. Absence here is a not-found input error
        // (exit 1): the caller asked about something that is not in this
        // repository. Absence of an object a RECEIPT names is corruption (exit
        // 2): the graph claims an edge it cannot produce. Collapsing the two
        // would report a typo as a damaged repository.
        let named_type = decode_object_type(&self.store.get_raw_verified(named)?)?;
        let (result, receipt_oid) = match named_type {
            ObjectType::Contribution => (None, named),
            ObjectType::Commit | ObjectType::Snapshot => {
                let (commit_oid, commit) = self.peel_commit(spec)?;
                let contrib = commit.contrib.ok_or_else(|| {
                    Error::NotFound(format!(
                        "{spec} names commit {commit_oid}, which carries no contribution \
                         receipt; a merge commit and a canonical historical commit both have \
                         none, and I10 makes that absence legitimate rather than corrupt"
                    ))
                })?;
                (Some((commit_oid, commit)), contrib)
            }
            other => {
                return Err(Error::Invalid(format!(
                    "{spec} is {}, which is neither a receipt nor a commit that names one",
                    other.as_str()
                )))
            }
        };
        self.verified_receipt(result, receipt_oid)
    }

    /// I25: check every edge before reporting any of them.
    fn verified_receipt(
        &self,
        result: Option<(ObjectId, Commit)>,
        receipt_oid: ObjectId,
    ) -> Result<Receipt> {
        let bytes = self.durable_bytes(receipt_oid, "receipt", "receipt")?;
        let contribution = Contribution::decode(&bytes).map_err(|error| {
            Error::Corrupt(format!("receipt {receipt_oid} does not decode: {error}"))
        })?;

        self.require_durable_type(contribution.base, ObjectType::Commit, "base", receipt_oid)?;
        self.require_durable_type(contribution.tree, ObjectType::Tree, "tree", receipt_oid)?;
        for parent in &contribution.parents {
            self.require_durable_type(*parent, ObjectType::Commit, "parent", receipt_oid)?;
        }
        for read in &contribution.reads {
            // Only blob observations reach a receipt: a directory or an
            // absence is not representable in VERSION 1 `reads`, so anything
            // here that is not a blob is a corrupt receipt and not a shape
            // this version can legitimately produce.
            self.require_durable_type(read.id, ObjectType::Blob, "read", receipt_oid)?;
        }

        // A receipt reached through a commit is a claim ABOUT that commit. The
        // three fields the publishing checkin copies into both objects must
        // still agree, or the receipt describes work some other commit
        // published.
        if let Some((commit_oid, commit)) = &result {
            let disagreement = if commit.tree != contribution.tree {
                Some(format!(
                    "tree {} != receipt tree {}",
                    commit.tree, contribution.tree
                ))
            } else if commit.parents != contribution.parents {
                Some("parents differ".to_string())
            } else if commit.agent != contribution.agent {
                Some(format!(
                    "agent {:?} != receipt agent {:?}",
                    commit.agent, contribution.agent
                ))
            } else {
                None
            };
            if let Some(detail) = disagreement {
                return Err(Error::Corrupt(format!(
                    "commit {commit_oid} disagrees with the receipt {receipt_oid} it names: \
                     {detail}"
                )));
            }
        }

        Ok(Receipt {
            receipt: receipt_oid,
            result: result.map(|(oid, _)| oid),
            agent: contribution.agent,
            base: contribution.base,
            tree: contribution.tree,
            parents: contribution.parents,
            reads: contribution
                .reads
                .into_iter()
                .map(|read| (read.path, read.id))
                .collect(),
            writes: contribution.writes,
            ts: contribution.ts,
        })
    }

    /// Durable bytes for one edge of a receipt, with absence reported as the
    /// corrupt graph it is rather than as a missing file (I15, I25).
    fn durable_bytes(&self, oid: ObjectId, edge: &str, of: &str) -> Result<Vec<u8>> {
        self.store
            .get_raw_verified(oid)
            .map_err(|error| match error {
                Error::NotFound(_) => Error::Corrupt(format!(
                    "{of} names {edge} {oid}, which is not present in durable storage"
                )),
                other => other,
            })
    }

    /// The type of an object a RECEIPT names. Absence is corruption here; see
    /// `receipt` for why the object the caller named is treated differently.
    fn durable_edge_type(&self, oid: ObjectId, edge: &str, of: &str) -> Result<ObjectType> {
        let bytes = self.durable_bytes(oid, edge, of)?;
        decode_object_type(&bytes)
    }

    fn require_durable_type(
        &self,
        oid: ObjectId,
        want: ObjectType,
        edge: &str,
        receipt: ObjectId,
    ) -> Result<()> {
        let found = self.durable_edge_type(oid, edge, &format!("receipt {receipt}"))?;
        if found != want {
            return Err(Error::Corrupt(format!(
                "receipt {receipt} names {edge} {oid} as {}, but it is {}",
                want.as_str(),
                found.as_str()
            )));
        }
        Ok(())
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
