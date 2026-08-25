//! Explicit retirement (`abandon`) and the reachability plan behind `gc`.
//!
//! Issues #12 and #309. A contended round mints one
//! `heads/agents/<agent>/forks/<ref>/<ulid>` (#343)
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
//! Collection is [`Forge::gc_collect`]. The plan-only path [`Forge::gc`] is
//! unchanged and still refuses to delete; the invariant the sweep preserves,
//! and the one precondition it cannot prove for itself, are stated on
//! `gc_collect`.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_ns::{parse_spec, Spec};
use forge_store::{
    decode_graph_object, GcCatalogRoots, GraphEdge, GraphExpectation, GraphWorkQueue, RetiredRef,
};
use forge_types::{Error, ObjectId, ObjectType, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
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

/// The smallest grace floor `--collect` will accept.
///
/// The floor is not decoration. A sweep is safe against every *published*
/// root, because it reads roots and unlinks inside one catalog write
/// transaction (see [`Forge::gc_collect`]). What it cannot see is a writer
/// that has already put or deduplicated an object and has not yet reached the
/// transaction that names it: that object is on disk, reachable from nothing,
/// and about to become live. The floor is the bound on that window, so it must
/// exceed the longest single put-to-publish interval in the deployment. Five
/// The bound is not "how long a checkin takes", it is how long a checkin can
/// *block*: the catalog's `busy_timeout` is five seconds per write
/// transaction, and a soak of six contended writers measured a single
/// put-to-publish interval of 5.08s -- one full timeout -- against a median in
/// the low milliseconds. A floor at that measured tail would be no floor at
/// all, so the minimum is an order of magnitude above it. The default stays a
/// day, and this is a hard minimum rather than a recommendation.
pub const GC_COLLECT_MIN_AGE_FLOOR: u64 = 60;

/// The most objects one `--collect` unlinks before stopping and saying so.
///
/// A sweep holds the catalog write lock and the object plane's reclamation
/// lock, so every writer in every process blocks on it. Without a cap that
/// stall grows with the size of the garbage heap, which is the wrong way round:
/// the bigger the backlog, the longer the outage -- and the stall lands on the
/// put-to-publish interval, which is the exact quantity the age floor has to
/// bound. A 120-second soak with an uncapped sweep measured that interval at
/// 52s against a 60s floor. The cap turns "one unbounded outage" into "as many
/// short ones as the operator chooses to run", and `batch_limited` in the
/// report is how they know to run another.
pub const GC_COLLECT_BATCH_LIMIT: usize = 4_096;

/// How many collectable object ids the report lists before truncating.
pub const GC_SAMPLE_LIMIT: usize = 256;

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct GcRootCounts {
    pub refs: usize,
    /// Unresolved forked contributions: `forks/*` (merge and import) plus
    /// `heads/agents/<agent>/forks/*` (session checkin, #343).
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
    /// False only for [`Forge::gc_collect`], the one path that unlinks.
    pub dry_run: bool,
    pub min_age_secs: u64,
    pub roots: GcRootCounts,
    /// Distinct object ids reached from the root set.
    pub reachable_objects: usize,
    /// Object files found under `objects/`.
    pub scanned_objects: usize,
    /// Unreachable and older than `min_age_secs`: what collection would
    /// delete, and for `gc_collect` what it did delete.
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
    /// The sweep stopped at [`GC_COLLECT_BATCH_LIMIT`]; run it again.
    pub batch_limited: bool,
}

impl GcReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "gc ({}, min-age {}s): {} of {} objects reachable\n",
            if self.dry_run { "dry-run" } else { "collect" },
            self.min_age_secs,
            self.reachable_objects,
            self.scanned_objects
        ));
        out.push_str(&format!(
            "roots: {} refs ({} unresolved forks), {} session pins, {} live refs, {} mounts, \
             {} mount pins, {} overlay blobs, {} observations, {} landmarks, {} seals\n",
            self.roots.refs,
            self.roots.unresolved_forks,
            self.roots.session_pins,
            self.roots.session_live_refs,
            self.roots.mounts,
            self.roots.mount_pins,
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
        if self.dry_run {
            out.push_str("nothing was deleted; this was a dry run\n");
        } else {
            out.push_str(&format!(
                "collected: {} objects unlinked\n",
                self.deleted_objects
            ));
            if self.batch_limited {
                out.push_str(&format!(
                    "stopped at the {GC_COLLECT_BATCH_LIMIT}-object batch limit; run gc again\n"
                ));
            }
        }
        out
    }
}

struct ScannedObject {
    id: ObjectId,
    path: PathBuf,
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
        schedule_catalog_roots(&self.store.meta.gc_roots()?, &mut roots, &mut queue)?;

        let mut reachable = HashSet::new();
        walk(self, queue, &mut reachable)?;

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
            batch_limited: false,
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

    /// Reclaim unreachable objects. This is the path that deletes bytes.
    ///
    /// # I23, and how the concurrent sweep race is closed
    ///
    /// The race that makes a naive collector unsound is not finding garbage,
    /// it is that garbage stops being garbage while you look at it: `gc`
    /// decides X is unreachable, a session checks in a tree naming X, `gc`
    /// unlinks X, and the published ref now points at a tree whose child does
    /// not exist -- I4 broken, silently. Content addressing makes this *more*
    /// likely, not less: a writer that reproduces bytes X already holds does
    /// not rewrite them (I3), so a perfectly ordinary checkin can start naming
    /// an object that has looked like cold garbage for a month.
    ///
    /// Three mechanisms close it, and all three are load-bearing.
    ///
    /// 1. **Roots are read and objects unlinked in one catalog write
    ///    transaction.** [`forge_store::Meta::gc_sweep`] holds `BEGIN
    ///    IMMEDIATE`, the same cross-process SQLite write lock every root
    ///    publication commits under -- `cas_ref`, `set_pin`, `overlay_upsert`,
    ///    `commit_seal`. No root can appear between the mark and the sweep,
    ///    from this process or any other, because no root can be published at
    ///    all while the sweep runs. This is the durable collection epoch
    ///    `docs/GC.md` said a collector needed.
    /// 2. **A deduplicating put refreshes the object's age.** That is the
    ///    content-addressing half. Without it, "old" means "written long ago",
    ///    which says nothing about whether a writer is relying on the object
    ///    right now. With it, "old" means "no writer has written or joined
    ///    these bytes for `min_age_secs`", which is the statement the floor
    ///    has to make. See `refresh_dedup_mtime` in `forge-store`.
    /// 3. **Every candidate's age is re-read inside the transaction, at the
    ///    last moment before its unlink, under the object plane's own
    ///    exclusive lock.** The scan that shortlists candidates runs unlocked
    ///    and can be arbitrarily stale; a dedup that lands after it still
    ///    saves the object, because the age that decides is the one read under
    ///    the lock. The lock is what makes that airtight rather than merely
    ///    likely: without it there is a window of a few microseconds, between
    ///    reading an age and acting on it, in which a publisher can join the
    ///    object -- small, but a sweep performs it once per candidate and a
    ///    contended repository produces thousands of candidates a minute.
    ///
    /// Ordering is the fourth: `objects/` is scanned *before* the first root
    /// read, so an object created after the scan is never a candidate, and an
    /// object that became reachable between the two is seen by the walk. The
    /// walk itself fails closed -- an unreadable or undecodable object aborts
    /// the whole sweep with `Corrupt` rather than misreporting its children as
    /// garbage.
    ///
    /// # The precondition this cannot prove
    ///
    /// A writer that put or deduplicated an object more than `min_age_secs`
    /// ago and has *still* not reached the transaction that names it is
    /// outside every mechanism above: its object is on disk, reachable from
    /// nothing, and older than the floor. ForgeFS has no session lease to
    /// bound that (`docs/GC.md`, gap 1), so the bound is the floor itself, and
    /// collection is sound exactly while no single put-to-publish interval
    /// exceeds `min_age_secs`. [`GC_COLLECT_MIN_AGE_FLOOR`] is the hard
    /// minimum; [`DEFAULT_MIN_AGE_SECS`] is a day. Note what is *not* in this
    /// precondition: a session that has held a pin for a month is perfectly
    /// safe, because a pin is a catalog row and therefore a root (I8).
    ///
    /// An ObjectId held outside the repository -- written down by an operator,
    /// passed between tools -- is not a root and never was. `landmark` is the
    /// verb that makes one a root.
    ///
    /// Requires unrestricted read authority for the same reason [`Forge::gc`]
    /// does (a filtered ref view is a filtered root set), and write authority
    /// because unlinking an object is the least recoverable act in the system.
    pub fn gc_collect(&self, cap: &Cap, min_age_secs: u64) -> Result<GcReport> {
        self.check(cap, Op::Read, None)?;
        self.check(cap, Op::Write, None)?;
        if !cap.has_unrestricted_ref_scope() {
            return Err(Error::Denied(
                "gc requires unrestricted read authority; a filtered ref view is not a root set"
                    .into(),
            ));
        }
        if min_age_secs < GC_COLLECT_MIN_AGE_FLOOR {
            return Err(Error::Invalid(format!(
                "gc --collect requires --min-age-secs >= {GC_COLLECT_MIN_AGE_FLOOR}: the floor \
                 bounds the window in which a writer has put an object and not yet published a \
                 root naming it, and a floor below it collects live data (see docs/GC.md)"
            )));
        }

        // Scan first: an object created after this point is not a candidate,
        // and one that becomes reachable after it is seen by a root walk.
        let mut unnamed_files = 0usize;
        let scanned = scan_objects(&self.store.root(), &mut unnamed_files)?;

        // Pass one, unlocked. This is the expensive walk, and it exists only
        // to shrink the work the write transaction has to do; nothing is
        // decided here.
        let mut warm = GcRootCounts::default();
        let mut queue = GraphWorkQueue::default();
        let first_roots = self.store.meta.gc_roots()?;
        schedule_catalog_roots(&first_roots, &mut warm, &mut queue)?;
        let mut reachable = HashSet::new();
        walk(self, queue, &mut reachable)?;
        let shortlist: Vec<&ScannedObject> = scanned
            .iter()
            .filter(|object| !reachable.contains(&object.id))
            .collect();

        // Pass two, under the write transaction. Roots are re-read here, and
        // `reachable` is carried over as the visited set so shared subtrees
        // are not re-walked: a root that pass one already covered adds
        // nothing, and only genuinely new roots cost anything. Carrying it
        // over is also conservative in the safe direction -- an object that
        // pass one reached and pass two would not is kept, never swept.
        let min_age = Duration::from_secs(min_age_secs);
        self.store.meta.gc_sweep(|sweep| {
            let mut roots = GcRootCounts::default();
            let mut queue = GraphWorkQueue::default();
            schedule_catalog_roots(&sweep.roots()?, &mut roots, &mut queue)?;
            walk(self, queue, &mut reachable)?;

            let mut report = GcReport {
                dry_run: false,
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
                batch_limited: false,
            };
            // Choosing what to unlink is not per-object. `fsck --full` roots
            // every object *file*, not just the catalog's roots, so it walks
            // out of surviving garbage too: unlink a contribution and leave
            // the garbage commit that names it, and fsck reports OBJECT_READ.
            // "Reclaimed space converted into reported corruption" is the
            // failure docs/GC.md named, and a per-object rule produces it
            // whenever a subgraph splits across the age floor or the batch
            // limit -- which under a concurrent load is constantly.
            //
            // So the doomed set is closed by construction: everything that
            // survives this sweep is walked, and anything it can reach is
            // spared. Age and the batch limit decide what is *offered* for
            // collection; this decides what is safe to take.
            let mut sample = Vec::new();
            // The object plane's own exclusion, held across every age check
            // and every unlink below. The catalog transaction stops roots from
            // being *published* during the sweep; this stops a deduplicating
            // put from refreshing an object's age in the microseconds between
            // this sweep reading that age and acting on it. Both are needed:
            // the first covers the graph, the second covers the clock.
            //
            // Held across the whole loop rather than taken per candidate.
            // Per-candidate looked strictly better -- it shortens how long a
            // publisher can block -- and a 120s soak found dangling references
            // with it, so it is not an option: the exclusion has to cover the
            // sweep, not each of its steps. `GC_COLLECT_BATCH_LIMIT` is what
            // bounds the stall.
            let _objects = self.store.gc_exclusive_objects()?;
            let now = SystemTime::now();
            let mut doomed = Vec::new();
            let mut spared = Vec::new();
            for object in &shortlist {
                if reachable.contains(&object.id) {
                    continue;
                }
                // The age that decides is read here, under that lock and
                // inside the transaction: a deduplicating put that landed
                // after the unlocked scan has refreshed it, and that is the
                // whole content-addressing defence. A vanished or unreadable
                // entry is simply skipped -- this sweep is not the only thing
                // entitled to have run.
                let Ok(metadata) = fs::metadata(&object.path) else {
                    continue;
                };
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .unwrap_or_default();
                if age < min_age || doomed.len() >= GC_COLLECT_BATCH_LIMIT {
                    if age < min_age {
                        report.withheld_young_objects += 1;
                        report.withheld_young_bytes =
                            report.withheld_young_bytes.saturating_add(metadata.len());
                    } else {
                        report.batch_limited = true;
                    }
                    spared.push(*object);
                    continue;
                }
                doomed.push((*object, metadata.len()));
            }

            // Walk out of everything that survives. Leniently: a spared
            // object may already have a missing child from an earlier sweep or
            // a crash, and there is nothing left to protect down that edge.
            // The root walk above is the one that must fail closed, and it
            // already did.
            protect_from_survivors(self, &spared, &mut reachable);
            report.reachable_objects = reachable.len();

            for (object, size) in &doomed {
                if reachable.contains(&object.id) {
                    continue;
                }
                // Catalog rows first. They commit with the transaction, so if
                // the unlink below fails the rows come back with it; the
                // reverse order could leave a row naming bytes that are gone.
                sweep.forget_object(object.id)?;
                match fs::remove_file(&object.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(Error::Io(format!(
                            "gc could not unlink {}: {error}",
                            object.id
                        )))
                    }
                }
                // The LRU caches assume immutability, which is true of an
                // object's bytes and false of its existence.
                self.store.forget_cached(object.id);
                report.deleted_objects += 1;
                report.collectable_objects += 1;
                report.collectable_bytes = report.collectable_bytes.saturating_add(*size);
                sample.push(object.id.hex());
            }
            sample.sort();
            report.sample_truncated = sample.len() > GC_SAMPLE_LIMIT;
            sample.truncate(GC_SAMPLE_LIMIT);
            report.collectable_sample = sample;
            Ok(report)
        })
    }
}

/// Turn one consistent catalog snapshot into the reclamation root set.
///
/// Both `gc` and `gc_collect` go through here so the reporting root set and
/// the deleting root set cannot drift apart: a root the report knows about and
/// the sweep does not is precisely how a collector eats live data.
fn schedule_catalog_roots(
    catalog: &GcCatalogRoots,
    counts: &mut GcRootCounts,
    queue: &mut GraphWorkQueue,
) -> Result<()> {
    for (name, _kind, oid) in &catalog.refs {
        counts.refs += 1;
        // Both fork spellings: a session fork lives under the agent's own
        // subtree so its author can act through the retargeted mount (#343),
        // and it is exactly as unresolved as a merge fork under `forks/`.
        if forge_store::meta::is_fork_ref(name) {
            counts.unresolved_forks += 1;
        }
        schedule_root(queue, *oid, GraphExpectation::Any, format!("ref:{name}"))?;
    }

    for (ns, pin) in &catalog.pins {
        counts.session_pins += 1;
        schedule_root(
            queue,
            *pin,
            GraphExpectation::Exact(ObjectType::Commit),
            format!("namespace:{ns}:pin"),
        )?;
    }
    // The live ref was already rooted by the refs pass; count it so an
    // operator can see the shape of the root set.
    counts.session_live_refs += catalog.live_refs;

    for (ns, path, spec, base) in &catalog.mounts {
        counts.mounts += 1;
        // A raw-OID mount can root an object no ref names.
        if let Ok(Spec::Oid(id)) = parse_spec(spec) {
            schedule_root(
                queue,
                id,
                GraphExpectation::Any,
                format!("namespace:{ns}:mount:{path}"),
            )?;
        }
        // I19: a read-write mount's pinned base is a root in its OWN right,
        // and this is the one that matters for collection. A `ref:` mount is
        // NOT covered by the refs pass: the refs pass roots what the ref holds
        // NOW, and the moment another agent advances that ref, this pin is the
        // only thing keeping the tree the mount still serves -- and still folds
        // at checkin -- reachable. Sweeping it would unlink the base of a live
        // mount, which is I19's own tree and I18's staged work, exactly as a
        // collector could once have reclaimed a session pin.
        if let Some(id) = base {
            counts.mount_pins += 1;
            schedule_root(
                queue,
                *id,
                GraphExpectation::Any,
                format!("namespace:{ns}:mount:{path}:base"),
            )?;
        }
    }

    for (ns, path, oid) in &catalog.overlay {
        counts.overlay_blobs += 1;
        schedule_root(
            queue,
            *oid,
            GraphExpectation::Exact(ObjectType::Blob),
            format!("namespace:{ns}:overlay:{path}"),
        )?;
    }

    for (ns, path, kind, oid) in &catalog.observations {
        let expected = match kind.as_str() {
            "blob" => ObjectType::Blob,
            "tree" => ObjectType::Tree,
            // An observation that names bytes under an unknown discriminant
            // has an unknowable type, and guessing would either misreport a
            // root or drop one. Fail the whole computation.
            other => {
                return Err(Error::Corrupt(format!(
                    "observation namespace:{ns}:{path} names an object under unusable kind {other}"
                )))
            }
        };
        counts.observations += 1;
        schedule_root(
            queue,
            *oid,
            GraphExpectation::Exact(expected),
            format!("namespace:{ns}:observation:{path}"),
        )?;
    }

    for (oid, _kind) in &catalog.landmarks {
        counts.landmarks += 1;
        schedule_root(
            queue,
            *oid,
            GraphExpectation::Any,
            format!("landmark:{}", oid.hex()),
        )?;
    }

    for (tag, snap, commit, tree) in &catalog.seals {
        counts.seals += 1;
        schedule_root(
            queue,
            *snap,
            GraphExpectation::Exact(ObjectType::Snapshot),
            format!("seal:{tag}:snapshot"),
        )?;
        schedule_root(
            queue,
            *commit,
            GraphExpectation::Exact(ObjectType::Commit),
            format!("seal:{tag}:commit"),
        )?;
        schedule_root(
            queue,
            *tree,
            GraphExpectation::Exact(ObjectType::Tree),
            format!("seal:{tag}:tree"),
        )?;
    }
    Ok(())
}

/// Spare everything a surviving object can reach.
///
/// This is the counterpart to [`walk`]'s fail-closed stance, and it is
/// deliberately the opposite shape. `walk` proves what is live and must abort
/// on an object it cannot read, because a subtree silently dropped from the
/// reachable set is a subtree handed to a collector. This one only ever *adds*
/// to the spared set, so an unreadable edge costs nothing: there is nothing
/// down it left to protect. Being lenient is what lets a sweep run at all in a
/// repository that already contains garbage with missing children.
fn protect_from_survivors(
    forge: &Forge,
    survivors: &[&ScannedObject],
    reachable: &mut HashSet<ObjectId>,
) {
    let mut pending: Vec<ObjectId> = survivors
        .iter()
        .filter(|object| !reachable.contains(&object.id))
        .map(|object| object.id)
        .collect();
    let mut seen: HashSet<ObjectId> = pending.iter().copied().collect();
    while let Some(id) = pending.pop() {
        reachable.insert(id);
        let Ok(bytes) = forge.store.get_raw_verified(id) else {
            continue;
        };
        let Ok(decoded) = decode_graph_object(id, &bytes) else {
            continue;
        };
        for edge in decoded.edges {
            if seen.insert(edge.id) {
                pending.push(edge.id);
            }
        }
    }
}

/// Reachability must fail closed. `fsck` records a bad object as a finding and
/// carries on, which is right for a report; here it would silently drop a
/// subtree from the reachable set and hand a collector live objects to delete.
fn walk(forge: &Forge, mut queue: GraphWorkQueue, reachable: &mut HashSet<ObjectId>) -> Result<()> {
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
    Ok(())
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
                    path: file.path(),
                    size: metadata.len(),
                    age,
                });
            }
        }
    }
    Ok(out)
}
