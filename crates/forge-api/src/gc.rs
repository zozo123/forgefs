//! Explicit retirement (`abandon`) and the reachability plan behind `gc`.
//!
//! Issues #12 and #309. A contended round mints one `forks/<ref>/<agent>/<ulid>`
//! ref per losing CAS (I18), each pinning a whole object closure, and nothing
//! retired them: unbounded growth on the steady-state path. The tension is that
//! a naive reachability sweep would resolve it in exactly the wrong direction --
//! fork refs *are* the staged work I18 promises never to destroy.
//!
//! The resolution is that a fork is a GC root until it is explicitly resolved.
//! Merging resolves it implicitly (its objects become reachable from the target
//! ref, so the fork ref roots nothing new). Abandoning resolves it explicitly,
//! and that is the verb this module adds. Because `abandon_ref` removes the
//! `refs` row, "unresolved forks are roots" needs no special case in the root
//! set: it falls out of "every ref is a root".
//!
//! Collection itself is NOT implemented. `gc` computes and reports a plan and
//! refuses to delete; see the module docs on [`Forge::gc`] for what a safe
//! collector still needs.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_ns::{parse_spec, Spec};
use forge_store::{
    decode_graph_object, GraphEdge, GraphExpectation, GraphWorkQueue, Observed, RetiredRef,
};
use forge_types::{Error, ObjectId, ObjectType, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Objects younger than this are never reported as collectable.
///
/// An object is fsynced into `objects/` *before* the catalog row that roots it
/// (I4: a committed ref implies durable bytes, never the reverse), so there is
/// always a window in which live bytes are reachable from nothing. A collector
/// that ignores that window deletes an object a concurrent session is about to
/// pin. ForgeFS has no session lease today, so this floor is a stand-in for one
/// and is deliberately far larger than any plausible put-to-commit gap.
pub const DEFAULT_MIN_AGE_SECS: u64 = 86_400;

/// How many collectable object ids the report lists before truncating.
pub const GC_SAMPLE_LIMIT: usize = 256;

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct GcRootCounts {
    pub refs: usize,
    /// Refs under `forks/`, i.e. unresolved forked contributions.
    pub unresolved_forks: usize,
    pub session_pins: usize,
    pub session_live_refs: usize,
    pub mounts: usize,
    /// Read-write mounts carrying their own pinned base (I19). Each one is a
    /// root the refs pass does not cover once its ref has moved on.
    pub mount_pins: usize,
    pub overlay_blobs: usize,
    pub observations: usize,
    pub landmarks: usize,
    pub seals: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GcReport {
    /// Always true today: `gc` refuses to collect. See [`Forge::gc`].
    pub dry_run: bool,
    pub min_age_secs: u64,
    pub roots: GcRootCounts,
    /// Distinct object ids reached from the root set.
    pub reachable_objects: usize,
    /// Object files found under `objects/`.
    pub scanned_objects: usize,
    /// Unreachable and older than `min_age_secs`: what collection would delete.
    pub collectable_objects: usize,
    pub collectable_bytes: u64,
    /// Unreachable but too young to be provably garbage; withheld.
    pub withheld_young_objects: usize,
    pub withheld_young_bytes: u64,
    /// Files under `objects/` whose name is not an ObjectId. Never collectable.
    pub unnamed_files: usize,
    pub deleted_objects: usize,
    /// Up to [`GC_SAMPLE_LIMIT`] collectable ids, sorted, for review.
    pub collectable_sample: Vec<String>,
    pub sample_truncated: bool,
}

impl GcReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "gc (dry-run, min-age {}s): {} of {} objects reachable\n",
            self.min_age_secs, self.reachable_objects, self.scanned_objects
        ));
        out.push_str(&format!(
            "roots: {} refs ({} unresolved forks), {} session pins, {} live refs, {} mounts, \
             {} overlay blobs, {} observations, {} landmarks, {} seals\n",
            self.roots.refs,
            self.roots.unresolved_forks,
            self.roots.session_pins,
            self.roots.session_live_refs,
            self.roots.mounts,
            self.roots.overlay_blobs,
            self.roots.observations,
            self.roots.landmarks,
            self.roots.seals
        ));
        out.push_str(&format!(
            "collectable: {} objects, {} bytes\n",
            self.collectable_objects, self.collectable_bytes
        ));
        out.push_str(&format!(
            "withheld (younger than min-age): {} objects, {} bytes\n",
            self.withheld_young_objects, self.withheld_young_bytes
        ));
        if self.unnamed_files > 0 {
            out.push_str(&format!(
                "unnamed files under objects/: {} (never collectable)\n",
                self.unnamed_files
            ));
        }
        for id in &self.collectable_sample {
            out.push_str(&format!("collectable {id}\n"));
        }
        if self.sample_truncated {
            out.push_str("... sample truncated\n");
        }
        out.push_str("nothing was deleted; collection is not implemented\n");
        out
    }
}

struct ScannedObject {
    id: ObjectId,
    size: u64,
    age: Duration,
}

impl Forge {
    /// Retire a fork ref: it stops being a GC root and its name is retired.
    ///
    /// Requires write authority over the ref itself, so the agent that forked
    /// can retire its own fork without any broader grant.
    pub fn abandon_fork(&self, cap: &Cap, name: &str) -> Result<RetiredRef> {
        self.check(cap, Op::Write, Some(name))?;
        self.store.meta.abandon_ref(name, cap.agent_id())
    }

    /// Retire a session: its pin, mounts, overlay and observations stop being
    /// GC roots. This is the escape hatch a stranded session never had.
    ///
    /// Refuses while the session holds staged overlay entries unless
    /// `discard_staged` is set, so no accidental path destroys staged work.
    pub fn abandon_session(
        &self,
        cap: &Cap,
        ns: &str,
        discard_staged: bool,
    ) -> Result<forge_store::AbandonedSession> {
        self.require_ns(cap, ns)?;
        self.store.meta.abandon_session(ns, discard_staged)
    }

    /// Compute what a collector would reclaim, and report it. Never deletes.
    ///
    /// The root set is refs UNION live session pins (plus their live refs,
    /// mounts, overlay blobs and observations) UNION landmarks UNION seals.
    /// Unresolved forks need no separate rule: they are refs. `abandon_fork` is
    /// what takes one out of the set.
    ///
    /// Two ordering properties make the plan sound as a *plan*:
    ///
    /// * the object directory is scanned BEFORE the roots are read, so an
    ///   object created after the scan is never a candidate, and an object that
    ///   became reachable between the two is seen by the root walk;
    /// * any object that cannot be read or decoded during the walk aborts the
    ///   whole computation with `Corrupt`, because an undecodable object hides
    ///   its outgoing edges and every child would be misreported as garbage.
    ///
    /// What is still missing before `--collect` could exist, and why this
    /// function refuses to delete:
    ///
    /// * there is no session lease. `min_age_secs` is a blunt stand-in for one:
    ///   it bounds the put-before-commit window (I4) but it does not bound how
    ///   long a session may hold an unwritten pin.
    /// * deletion and the root read are not one transaction. A collector needs
    ///   a durable collection epoch that new roots are stamped against, so a
    ///   root published after the walk cannot be collected by it.
    /// * `Store` keeps hot LRU object caches, so a collected object may still
    ///   be served from memory in the collecting process, which hides exactly
    ///   the bug a collector must not have.
    pub fn gc(&self, cap: &Cap, dry_run: bool, min_age_secs: u64) -> Result<GcReport> {
        self.check(cap, Op::Read, None)?;
        // A ref-scoped cap sees a filtered ref list, and a filtered root set is
        // the one input that turns garbage collection into data loss.
        if !cap.has_unrestricted_ref_scope() {
            return Err(Error::Denied(
                "gc requires unrestricted read authority; a filtered ref view is not a root set"
                    .into(),
            ));
        }
        if !dry_run {
            return Err(Error::Invalid(
                "gc supports --dry-run only; collection is not implemented (see docs/GC.md)".into(),
            ));
        }

        let mut unnamed_files = 0usize;
        let scanned = scan_objects(&self.store.root(), &mut unnamed_files)?;

        let mut roots = GcRootCounts::default();
        let mut queue = GraphWorkQueue::default();
        self.collect_gc_roots(&mut roots, &mut queue)?;

        let reachable = walk(self, queue)?;

        let min_age = Duration::from_secs(min_age_secs);
        let mut report = GcReport {
            dry_run: true,
            min_age_secs,
            roots,
            reachable_objects: reachable.len(),
            scanned_objects: scanned.len(),
            collectable_objects: 0,
            collectable_bytes: 0,
            withheld_young_objects: 0,
            withheld_young_bytes: 0,
            unnamed_files,
            deleted_objects: 0,
            collectable_sample: Vec::new(),
            sample_truncated: false,
        };
        let mut sample = Vec::new();
        for object in &scanned {
            if reachable.contains(&object.id) {
                continue;
            }
            if object.age < min_age {
                report.withheld_young_objects += 1;
                report.withheld_young_bytes =
                    report.withheld_young_bytes.saturating_add(object.size);
                continue;
            }
            report.collectable_objects += 1;
            report.collectable_bytes = report.collectable_bytes.saturating_add(object.size);
            sample.push(object.id.hex());
        }
        sample.sort();
        report.sample_truncated = sample.len() > GC_SAMPLE_LIMIT;
        sample.truncate(GC_SAMPLE_LIMIT);
        report.collectable_sample = sample;
        Ok(report)
    }

    fn collect_gc_roots(
        &self,
        counts: &mut GcRootCounts,
        queue: &mut GraphWorkQueue,
    ) -> Result<()> {
        for row in self.store.meta.list_refs()? {
            counts.refs += 1;
            if row.name.starts_with(forge_store::meta::ABANDONABLE_PREFIX) {
                counts.unresolved_forks += 1;
            }
            schedule_root(
                queue,
                row.oid,
                GraphExpectation::Any,
                format!("ref:{}", row.name),
            )?;
        }

        for ns in self.store.meta.list_namespaces()? {
            let resource = format!("namespace:{}", ns.id);
            if let Some(pin) = ns.pinned_oid {
                counts.session_pins += 1;
                schedule_root(
                    queue,
                    pin,
                    GraphExpectation::Exact(ObjectType::Commit),
                    format!("{resource}:pin"),
                )?;
            }
            if ns.live_ref.is_some() {
                // The live ref was already rooted by the refs pass above; count
                // it so an operator can see the shape of the root set.
                counts.session_live_refs += 1;
            }
            for mount in self.store.meta.list_mounts(&ns.id)? {
                counts.mounts += 1;
                // A `ref:` mount roots nothing new -- the refs pass covered it.
                // A raw-OID mount is the only mount that can root an object no
                // ref names, and it is the reason mounts are in the root set.
                if let Ok(Spec::Oid(id)) = parse_spec(&mount.spec) {
                    schedule_root(
                        queue,
                        id,
                        GraphExpectation::Any,
                        format!("{resource}:mount:{}", mount.path),
                    )?;
                }
                // I19: a read-write mount's pinned base is a root in its own
                // right. The refs pass roots what the ref holds NOW; once the
                // ref has moved on, this pin is the only thing keeping the tree
                // the mount still serves -- and still folds at checkin --
                // reachable. Without it a collector could reclaim the base of a
                // live mount, exactly as it could once have reclaimed a session
                // pin.
                if let Some(base) = mount.base_oid {
                    counts.mount_pins += 1;
                    schedule_root(
                        queue,
                        base,
                        GraphExpectation::Any,
                        format!("{resource}:mount:{}:base", mount.path),
                    )?;
                }
                for row in self.store.meta.overlay_list(&ns.id, &mount.path)? {
                    if let Some(id) = row.blob_oid {
                        counts.overlay_blobs += 1;
                        schedule_root(
                            queue,
                            id,
                            GraphExpectation::Exact(ObjectType::Blob),
                            format!("{resource}:overlay:{}", row.path),
                        )?;
                    }
                }
            }
            for observation in self.store.meta.observations(&ns.id)? {
                let expected = match observation.seen {
                    Observed::Blob(_) => ObjectType::Blob,
                    Observed::Tree(_) => ObjectType::Tree,
                    Observed::Absent => continue,
                };
                let Some(id) = observation.seen.oid() else {
                    continue;
                };
                counts.observations += 1;
                schedule_root(
                    queue,
                    id,
                    GraphExpectation::Exact(expected),
                    format!("{resource}:observation:{}", observation.path),
                )?;
            }
        }

        for (oid, _kind) in self.store.meta.list_landmarks()? {
            counts.landmarks += 1;
            schedule_root(
                queue,
                oid,
                GraphExpectation::Any,
                format!("landmark:{}", oid.hex()),
            )?;
        }

        for (tag, snap, commit, tree) in self.store.meta.list_seals()? {
            counts.seals += 1;
            schedule_root(
                queue,
                snap,
                GraphExpectation::Exact(ObjectType::Snapshot),
                format!("seal:{tag}:snapshot"),
            )?;
            schedule_root(
                queue,
                commit,
                GraphExpectation::Exact(ObjectType::Commit),
                format!("seal:{tag}:commit"),
            )?;
            schedule_root(
                queue,
                tree,
                GraphExpectation::Exact(ObjectType::Tree),
                format!("seal:{tag}:tree"),
            )?;
        }
        Ok(())
    }
}

/// Reachability must fail closed. `fsck` records a bad object as a finding and
/// carries on, which is right for a report; here it would silently drop a
/// subtree from the reachable set and hand a collector live objects to delete.
fn walk(forge: &Forge, mut queue: GraphWorkQueue) -> Result<HashSet<ObjectId>> {
    let mut reachable = HashSet::new();
    while let Some(edge) = queue.pop_front() {
        if !reachable.insert(edge.id) {
            continue;
        }
        let bytes = forge.store.get_raw_verified(edge.id).map_err(|error| {
            Error::Corrupt(format!(
                "gc cannot prove reachability: {} ({}) is unreadable: {error}",
                edge.id, edge.resource
            ))
        })?;
        let decoded = decode_graph_object(edge.id, &bytes).map_err(|error| {
            Error::Corrupt(format!(
                "gc cannot prove reachability: {} ({}) does not decode: {error}",
                edge.id, edge.resource
            ))
        })?;
        for edge in decoded.edges {
            queue.schedule(edge)?;
        }
    }
    Ok(reachable)
}

fn schedule_root(
    queue: &mut GraphWorkQueue,
    id: ObjectId,
    expected: GraphExpectation,
    resource: String,
) -> Result<()> {
    queue.schedule(GraphEdge {
        id,
        expected,
        resource,
    })?;
    Ok(())
}

fn scan_objects(root: &Path, unnamed: &mut usize) -> Result<Vec<ScannedObject>> {
    let now = SystemTime::now();
    let objects = root.join("objects");
    let mut out = Vec::new();
    for a in fs::read_dir(&objects)? {
        let a = a?;
        if !a.file_type()?.is_dir() {
            *unnamed += 1;
            continue;
        }
        for b in fs::read_dir(a.path())? {
            let b = b?;
            if !b.file_type()?.is_dir() {
                *unnamed += 1;
                continue;
            }
            for file in fs::read_dir(b.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    *unnamed += 1;
                    continue;
                }
                let name = file.file_name().to_string_lossy().into_owned();
                let Ok(id) = ObjectId::from_hex(&name) else {
                    *unnamed += 1;
                    continue;
                };
                let metadata = file.metadata()?;
                // An unreadable or future mtime is treated as age zero, which
                // withholds the object instead of offering it for collection.
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .unwrap_or_default();
                out.push(ScannedObject {
                    id,
                    size: metadata.len(),
                    age,
                });
            }
        }
    }
    Ok(out)
}
