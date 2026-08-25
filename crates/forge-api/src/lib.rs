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
pub use fsck::{FsckFinding, FsckReport};
pub use gc::{
    GcReport, GcRootCounts, DEFAULT_MIN_AGE_SECS, GC_COLLECT_MIN_AGE_FLOOR, GC_SAMPLE_LIMIT,
};
pub use import::ImportOptions;
pub use repository::find_forge;
pub use serve::{dispatch as dispatch_request, serve, unix_worker_count};
pub use soak::{private_checkins_bounded, run_bench_with_workers, shared_stampede_bounded};
pub use stats::{
    ApiCounterReport, DurabilityReport, MetaCounterReport, StatsReport, StoreCounterReport,
    STATS_SCHEMA_VERSION, STATS_SCOPE,
};

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
}

#[derive(Debug, Default)]
struct ApiCounters {
    stale_observation: AtomicU64,
    merge_conflict: AtomicU64,
    sessions_opened: AtomicU64,
    merge_applied: AtomicU64,
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
        }
    }
}
