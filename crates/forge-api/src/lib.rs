//! The native Forge API. Agents speak this; POSIX is an adapter.

mod authority;
mod bench;
mod export;
mod fsck;
mod import;
mod integration;
mod refs;
mod repository;
mod serve;
mod soak;
mod test_hooks;
mod workspace;

pub use bench::{
    merge_all_and_seal, private_checkins, run as run_bench, shared_stampede, BenchReport,
};
pub use fsck::{FsckFinding, FsckReport};
pub use repository::find_forge;
pub use serve::{dispatch as dispatch_request, serve, unix_worker_count};
pub use soak::{private_checkins_bounded, run_bench_with_workers, shared_stampede_bounded};

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
    // `None` only for a read-only open whose media refused to hand out a LOCK descriptor.
    _cell_lock: Option<File>,
    exclusive_cell_lock: bool,
    read_only: bool,
}

impl Forge {
    pub fn api_stats(&self) -> ApiStats {
        ApiStats {
            stale_observation: self.stats.stale_observation.load(Ordering::Relaxed),
            merge_conflict: self.stats.merge_conflict.load(Ordering::Relaxed),
        }
    }
}
