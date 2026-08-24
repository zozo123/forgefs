//! I11/I12 deterministic integration and I15 sealed release verification.

use super::*;

impl Forge {
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
        if resolved.is_some() {
            return Err(Error::Invalid(RAW_MERGE_RESOLUTION_DISABLED.into()));
        }
        let ours_c = self.store.get_commit(into_row.oid)?;
        let (theirs_oid, theirs_c) = self.peel_commit(from)?;
        let tree = {
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
            contrib: None,
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
}
