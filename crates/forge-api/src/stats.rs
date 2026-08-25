//! One machine-readable surface for every counter this process kept.
//!
//! Evidence, never correctness policy. Nothing here gates a write, and no
//! number here is a measurement of a single operation: every field is a
//! monotonic total accumulated by *this* process since it opened the
//! repository, exactly as `forge bench` already renders them. Fresh processes
//! therefore report near-zero counters; that is the honest shape of the
//! machinery that exists, not a defect to paper over. Deriving a per-checkin
//! cost mix from these totals is wrong -- see `docs/BENCH.md`.
//!
//! The document is emitted by `forge stats --json` and its key set is a CLI
//! contract (`CLI_ABI.md`): keys are added, never renamed or removed, and
//! `schema_version` moves when that promise cannot be kept.

use crate::{ApiStats, Forge};
use forge_store::blob::BlobStoreStats;
use forge_store::{DurabilityPolicy, MetaStats};
use serde::Serialize;

/// Version of the `forge stats --json` key set, not of the repository.
///
/// 2: `txn_count` stopped under-reporting. It counted only explicit
/// `BEGIN IMMEDIATE` blocks, so an autocommit-only phase reported zero write
/// transactions (issue #311); it now counts every committed write transaction.
/// The key set is add-only as promised -- `explicit_txn_count` carries the old
/// number, and the lock pair was split into its write and read halves -- but a
/// series keyed on `txn_count` changes meaning across this boundary, and that
/// is exactly what the version exists to announce.
pub const STATS_SCHEMA_VERSION: u32 = 2;

/// The only scope these counters have ever had. Emitted as a field so a
/// consumer never has to infer it from documentation.
pub const STATS_SCOPE: &str = "process-lifetime";

const STATS_NOTE: &str = "Cumulative totals for this process only, from repository open to now. \
Not per-operation, not a benchmark, and never a source for a per-checkin cost mix.";

/// Durable object publication performed by this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StoreCounterReport {
    pub puts: u64,
    pub dedup_hits: u64,
    pub fsync_file: u64,
    pub fsync_file_us: u64,
    pub fsync_dir: u64,
    pub fsync_dir_us: u64,
    /// Saturating `fsync_file_us + fsync_dir_us`. Not wall time.
    pub barrier_us: u64,
}

/// Metadata catalog work: the SQLite transaction that is the visibility point,
/// and the ref CAS outcomes decided inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MetaCounterReport {
    /// Every write transaction SQLite committed: explicit blocks and
    /// autocommit statements alike. Not the denominator of `txn_us`.
    pub txn_count: u64,
    pub txn_us: u64,
    /// Explicit `BEGIN IMMEDIATE` attempts, the sample count behind `txn_us`.
    pub explicit_txn_count: u64,
    /// Summed over the write connection and the read pool, so neither is a
    /// writer-contention signal on its own; the split pair below is.
    pub lock_acquires: u64,
    pub lock_wait_us: u64,
    pub write_lock_acquires: u64,
    pub write_lock_wait_us: u64,
    pub read_lock_acquires: u64,
    pub read_lock_wait_us: u64,
    pub busy: u64,
    pub cas_updated: u64,
    pub cas_forked: u64,
    pub cas_denied: u64,
    pub cas_noop: u64,
    /// Saturating `lock_wait_us + txn_us`. Not wall time.
    pub accounted_us: u64,
}

/// Facade-level outcomes that no lower layer can attribute on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ApiCounterReport {
    pub sessions_opened: u64,
    pub stale_observation: u64,
    pub merge_applied: u64,
    pub merge_conflict: u64,
}

/// The catalog durability contract in force. Repository state, not a counter:
/// it is reported alongside the counters so nothing compares two runs that did
/// not promise the same thing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DurabilityReport {
    pub journal_mode: String,
    pub synchronous: i64,
    /// `null` where the platform has no `F_FULLFSYNC`.
    pub fullfsync: Option<bool>,
    /// True when the policy was only observed on a read-only open and nothing
    /// was established or enforced.
    pub read_only: bool,
}

/// The whole document emitted by `forge stats --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatsReport {
    pub schema_version: u32,
    /// Always [`STATS_SCOPE`].
    pub scope: &'static str,
    /// Prose restating `scope` for a human reading a raw dump.
    pub note: &'static str,
    pub durability: DurabilityReport,
    pub store: StoreCounterReport,
    pub sqlite: MetaCounterReport,
    pub api: ApiCounterReport,
}

impl StatsReport {
    fn build(
        store: BlobStoreStats,
        meta: MetaStats,
        api: ApiStats,
        durability: &DurabilityPolicy,
    ) -> Self {
        Self {
            schema_version: STATS_SCHEMA_VERSION,
            scope: STATS_SCOPE,
            note: STATS_NOTE,
            durability: DurabilityReport {
                journal_mode: durability.journal_mode.clone(),
                synchronous: durability.synchronous,
                fullfsync: durability.fullfsync,
                read_only: durability.read_only,
            },
            store: StoreCounterReport {
                puts: store.puts,
                dedup_hits: store.dedup_hits,
                fsync_file: store.fsync_file,
                fsync_file_us: store.fsync_file_us,
                fsync_dir: store.fsync_dir,
                fsync_dir_us: store.fsync_dir_us,
                barrier_us: store.barrier_us(),
            },
            sqlite: MetaCounterReport {
                txn_count: meta.txn_count,
                txn_us: meta.txn_us,
                explicit_txn_count: meta.explicit_txn_count,
                lock_acquires: meta.lock_acquires,
                lock_wait_us: meta.lock_wait_us,
                write_lock_acquires: meta.write_lock_acquires,
                write_lock_wait_us: meta.write_lock_wait_us,
                read_lock_acquires: meta.read_lock_acquires,
                read_lock_wait_us: meta.read_lock_wait_us,
                busy: meta.busy,
                cas_updated: meta.cas_updated,
                cas_forked: meta.cas_forked,
                cas_denied: meta.cas_denied,
                cas_noop: meta.cas_noop,
                accounted_us: meta.sqlite_accounted_us(),
            },
            api: ApiCounterReport {
                sessions_opened: api.sessions_opened,
                stale_observation: api.stale_observation,
                merge_applied: api.merge_applied,
                merge_conflict: api.merge_conflict,
            },
        }
    }

    /// Human rendering for `forge stats` without `--json`. Same numbers and
    /// the same scope disclaimer; the JSON document is the machine contract.
    pub fn render(&self) -> String {
        let fullfsync = match self.durability.fullfsync {
            Some(true) => "on",
            Some(false) => "off",
            None => "n/a",
        };
        format!(
            "forge stats schema={} scope={}\n\
             {}\n\
             durability       journal_mode={} synchronous={} fullfsync={} read_only={}\n\
             storage lifetime puts={} dedup_hits={} fsync_file={} fsync_file_us={} fsync_dir={} fsync_dir_us={} barrier_us={}\n\
             sqlite lifetime  lock_acquires={} lock_wait_us={} txn_count={} txn_us={} explicit_txn_count={} accounted_us={} busy={} updated={} forked={} denied={} noop={}\n\
             sqlite locks     write_acquires={} write_wait_us={} read_acquires={} read_wait_us={}\n\
             api lifetime     sessions_opened={} stale={} merge_applied={} conflict={}\n",
            self.schema_version,
            self.scope,
            self.note,
            self.durability.journal_mode,
            self.durability.synchronous,
            fullfsync,
            self.durability.read_only,
            self.store.puts,
            self.store.dedup_hits,
            self.store.fsync_file,
            self.store.fsync_file_us,
            self.store.fsync_dir,
            self.store.fsync_dir_us,
            self.store.barrier_us,
            self.sqlite.lock_acquires,
            self.sqlite.lock_wait_us,
            self.sqlite.txn_count,
            self.sqlite.txn_us,
            self.sqlite.explicit_txn_count,
            self.sqlite.accounted_us,
            self.sqlite.busy,
            self.sqlite.cas_updated,
            self.sqlite.cas_forked,
            self.sqlite.cas_denied,
            self.sqlite.cas_noop,
            self.sqlite.write_lock_acquires,
            self.sqlite.write_lock_wait_us,
            self.sqlite.read_lock_acquires,
            self.sqlite.read_lock_wait_us,
            self.api.sessions_opened,
            self.api.stale_observation,
            self.api.merge_applied,
            self.api.merge_conflict,
        )
    }
}

impl Forge {
    /// Snapshot every counter this process kept, plus the catalog durability
    /// policy they were produced under.
    ///
    /// Each counter family is read independently with relaxed loads. This is a
    /// diagnostic read, never a transaction: the families are not consistent
    /// to a single instant and no caller may treat them as if they were.
    pub fn stats_report(&self) -> StatsReport {
        StatsReport::build(
            self.store.stats(),
            self.store.meta.stats(),
            self.api_stats(),
            self.store.meta.durability_policy(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StatsReport {
        StatsReport::build(
            BlobStoreStats {
                puts: 2,
                dedup_hits: 1,
                fsync_file: 3,
                fsync_file_us: 11,
                fsync_dir: 4,
                fsync_dir_us: 13,
            },
            MetaStats {
                txn_us: 17,
                txn_count: 9,
                explicit_txn_count: 5,
                lock_wait_us: 19,
                lock_acquires: 23,
                write_lock_acquires: 20,
                write_lock_wait_us: 15,
                read_lock_acquires: 3,
                read_lock_wait_us: 4,
                busy: 1,
                cas_updated: 7,
                cas_forked: 2,
                cas_denied: 1,
                cas_noop: 4,
            },
            ApiStats {
                stale_observation: 2,
                merge_conflict: 3,
                sessions_opened: 6,
                merge_applied: 5,
            },
            &DurabilityPolicy {
                journal_mode: "wal".into(),
                synchronous: 2,
                fullfsync: None,
                read_only: false,
            },
        )
    }

    /// Derived totals must be the documented saturating sums, never wall time.
    #[test]
    fn derived_totals_are_sums_of_their_components() {
        let report = sample();
        assert_eq!(report.store.barrier_us, 11 + 13);
        assert_eq!(report.sqlite.accounted_us, 19 + 17);
    }

    /// A process-lifetime counter must never be presented as per-operation
    /// evidence (AGENTS.md test rules); the scope travels inside the document.
    #[test]
    fn document_carries_its_own_counter_scope() {
        let report = sample();
        assert_eq!(report.scope, "process-lifetime");
        assert!(report.note.contains("Not per-operation"));
        assert!(report.render().contains("scope=process-lifetime"));
    }
}
