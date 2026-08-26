//! The native Forge API. Agents speak this; POSIX is an adapter.

mod authority;
mod bench;
mod export;
mod fsck;
mod gc;
mod import;
mod integration;
mod refs;
mod repository;
mod serve;
mod soak;
mod stats;
mod test_hooks;
mod workspace;

pub use bench::{
    merge_all_and_seal, private_checkins, run as run_bench, shared_stampede, BenchReport,
};
pub use export::ExportOptions;
pub use fsck::{FsckFinding, FsckRefusal, FsckRefusalReason, FsckReport, FSCK_REFUSAL_SCHEMA};
pub use gc::{
    GcReport, GcRootCounts, DEFAULT_MIN_AGE_SECS, GC_COLLECT_MIN_AGE_FLOOR, GC_SAMPLE_LIMIT,
};
pub use import::ImportOptions;
pub use refs::Receipt;
pub use repository::find_forge;
pub use serve::{
    dispatch as dispatch_request, http_status as daemon_http_status, serve, unix_worker_count,
    DAEMON_OPS,
};
pub use soak::{
    private_checkins_bounded, read_fanout_bounded, run_bench_with_workers, shared_stampede_bounded,
};
pub use stats::{
    ApiCounterReport, CacheCounterReport, DurabilityReport, MetaCounterReport, StatsReport,
    StoreCounterReport, STATS_SCHEMA_VERSION, STATS_SCOPE,
};
// `counter_report` is gone: sections construct themselves through
// `StoreCounterReport::of` and friends, so `forge bench` can render the
// snapshots it has without needing the ones it does not.
pub use workspace::Renamed;

/// Stable fail-closed error for the legacy raw-tree merge resolution input.
///
/// A replacement tree is not sufficient proof that it resolves the conflict
/// produced by the current merge inputs. Keep the input in the API for
/// compatibility, but reject it until resolution carries a conflict OID and
/// durable provenance.
pub const RAW_MERGE_RESOLUTION_DISABLED: &str =
    "raw merge resolution is disabled; resolution must be bound to a conflict object";

use forge_store::Store;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApiStats {
    pub stale_observation: u64,
    pub merge_conflict: u64,
    /// I8 session pins established by `session_open`. A session is counted
    /// once when its namespace row exists; it is never decremented on close,
    /// because a process-lifetime counter has no close event to observe.
    pub sessions_opened: u64,
    /// Merges that produced a merge commit and reached the ref CAS. The CAS
    /// outcome itself belongs to the SQLite `cas_*` counters; conflicts that
    /// refused before any commit belong to `merge_conflict`.
    pub merge_applied: u64,
    /// Cumulative time inside merge-base search, across applied merges AND
    /// refused ones: the search runs before the outcome is known, so charging
    /// it only to successes would understate exactly the case that is slow.
    /// `merge_applied + merge_conflict` is NOT its sample count -- a merge can
    /// refuse before the search -- so never divide.
    pub merge_base_us: u64,
    /// Merge-base searches that ran. This IS the sample count for
    /// `merge_base_us`.
    pub merge_base_searches: u64,
    /// Moves staged by `rename` (I24). Counted once per accepted move
    /// whatever it moved, so it is a move count and never a file count.
    pub renames: u64,
    /// `gc` invocations that produced a report, dry runs included.
    pub gc_runs: u64,
    /// Objects a sweep actually unlinked, and their bytes. A dry run adds
    /// nothing to either: it deletes nothing, and a counter that could not
    /// tell the two apart would be worse than no counter (I23).
    pub gc_objects_deleted: u64,
    pub gc_bytes_deleted: u64,
    /// `fsck` invocations that produced a report, and the findings they
    /// produced. A clean repository moves `fsck_runs` and not `fsck_findings`,
    /// so the pair distinguishes "not checked" from "checked and clean" --
    /// which one number cannot.
    pub fsck_runs: u64,
    pub fsck_findings: u64,
}

#[derive(Debug, Default)]
struct ApiCounters {
    stale_observation: AtomicU64,
    merge_conflict: AtomicU64,
    sessions_opened: AtomicU64,
    merge_applied: AtomicU64,
    merge_base_us: AtomicU64,
    merge_base_searches: AtomicU64,
    renames: AtomicU64,
    gc_runs: AtomicU64,
    gc_objects_deleted: AtomicU64,
    gc_bytes_deleted: AtomicU64,
    fsck_runs: AtomicU64,
    fsck_findings: AtomicU64,
}

pub struct Forge {
    store: Store,
    hmac_key: [u8; 32],
    seal_seed: [u8; 32],
    seal_pk: [u8; 32],
    root: PathBuf,
    stats: ApiCounters,
    // Shared for direct clients, exclusive for the daemon. The descriptor lifetime is the lock.
    // `None` only for a read-only open whose media refused to hand out a LOCK descriptor.
    _cell_lock: Option<File>,
    exclusive_cell_lock: bool,
    read_only: bool,
    // True only when open deferred migration-ledger compatibility to full
    // fsck. The fsck call must use matching full mode or fail closed.
    fsck_catalog: bool,
}

impl Forge {
    pub fn api_stats(&self) -> ApiStats {
        ApiStats {
            stale_observation: self.stats.stale_observation.load(Ordering::Relaxed),
            merge_conflict: self.stats.merge_conflict.load(Ordering::Relaxed),
            sessions_opened: self.stats.sessions_opened.load(Ordering::Relaxed),
            merge_applied: self.stats.merge_applied.load(Ordering::Relaxed),
            merge_base_us: self.stats.merge_base_us.load(Ordering::Relaxed),
            merge_base_searches: self.stats.merge_base_searches.load(Ordering::Relaxed),
            renames: self.stats.renames.load(Ordering::Relaxed),
            gc_runs: self.stats.gc_runs.load(Ordering::Relaxed),
            gc_objects_deleted: self.stats.gc_objects_deleted.load(Ordering::Relaxed),
            gc_bytes_deleted: self.stats.gc_bytes_deleted.load(Ordering::Relaxed),
            fsck_runs: self.stats.fsck_runs.load(Ordering::Relaxed),
            fsck_findings: self.stats.fsck_findings.load(Ordering::Relaxed),
        }
    }

    /// Record one `gc` outcome. Called from the single place a `GcReport` is
    /// returned to a caller, so a new gc entry point cannot skip it.
    pub(crate) fn count_gc(&self, report: &GcReport) {
        self.stats.gc_runs.fetch_add(1, Ordering::Relaxed);
        self.stats
            .gc_objects_deleted
            .fetch_add(report.deleted_objects as u64, Ordering::Relaxed);
        if !report.dry_run {
            self.stats
                .gc_bytes_deleted
                .fetch_add(report.collectable_bytes, Ordering::Relaxed);
        }
    }

    /// Record one `fsck` outcome, at the single place a `FsckReport` reaches a
    /// caller.
    pub(crate) fn count_fsck(&self, report: &FsckReport) {
        self.stats.fsck_runs.fetch_add(1, Ordering::Relaxed);
        self.stats
            .fsck_findings
            .fetch_add(report.findings.len() as u64, Ordering::Relaxed);
    }

    /// Record one merge-base search and how long it took.
    pub(crate) fn count_merge_base(&self, elapsed: std::time::Duration) {
        self.stats
            .merge_base_searches
            .fetch_add(1, Ordering::Relaxed);
        let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        let _ = self.stats.merge_base_us.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_add(us)),
        );
    }

    pub(crate) fn count_rename(&self) {
        self.stats.renames.fetch_add(1, Ordering::Relaxed);
    }
}
