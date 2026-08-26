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
use forge_store::{DurabilityPolicy, MetaStats, StoreCacheStats};
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
    /// Filesystem-wide barriers this store executed. One of them stands in
    /// for the whole per-directory set of one or more batches, so it is a
    /// barrier count and never a batch count.
    pub barrier_fs: u64,
    pub barrier_fs_us: u64,
    /// Batches whose directory phase was satisfied by a filesystem-wide
    /// barrier, leader and followers alike. Divided by `barrier_fs` it is the
    /// achieved sharing depth.
    pub barrier_fs_batches: u64,
    /// Saturating `fsync_file_us + fsync_dir_us + barrier_fs_us`. Not wall
    /// time.
    pub barrier_us: u64,
    /// Object bytes written, summed over `puts`.
    pub put_bytes: u64,
    /// Object bytes a publication did not have to write, summed over
    /// `dedup_hits`. Paired with `put_bytes` this is the storage amplification
    /// content addressing avoided.
    pub dedup_bytes: u64,
    /// Object bytes read back from durable storage. Reads served from a cache
    /// never reach it, so this is physical read volume; the `cache` section
    /// explains the difference.
    pub get_bytes: u64,
    /// Durable object bytes that did not rehash to the id naming them. Every
    /// one is a refused read (I1, I3, I15).
    pub hash_failures: u64,
}

/// Hit and miss counts for the two hot in-process caches.
///
/// A separate section because it describes MEMORY, not durable work: nothing
/// here is a barrier, a transaction or a byte on disk, and mixing it into
/// `store` would let a cache hit look like avoided storage work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CacheCounterReport {
    pub object_cache_hits: u64,
    pub object_cache_misses: u64,
    pub tree_cache_hits: u64,
    pub tree_cache_misses: u64,
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
    /// Cumulative time inside merge-base search, over applied AND refused
    /// merges. `merge_base_searches` is its sample count; nothing else is.
    pub merge_base_us: u64,
    pub merge_base_searches: u64,
    /// Moves staged by `rename` (I24). One per accepted move, never per file.
    pub renames: u64,
    /// `gc` runs, dry runs included, and what collection actually unlinked.
    /// A dry run moves `gc_runs` alone: it deletes nothing.
    pub gc_runs: u64,
    pub gc_objects_deleted: u64,
    pub gc_bytes_deleted: u64,
    /// `fsck` runs and the findings they produced. A clean repository moves
    /// `fsck_runs` and not `fsck_findings`, so the pair separates "not
    /// checked" from "checked and clean".
    pub fsck_runs: u64,
    pub fsck_findings: u64,
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
    pub cache: CacheCounterReport,
    pub sqlite: MetaCounterReport,
    pub api: ApiCounterReport,
}

impl StoreCounterReport {
    pub fn of(store: BlobStoreStats) -> Self {
        Self {
            puts: store.puts,
            dedup_hits: store.dedup_hits,
            fsync_file: store.fsync_file,
            fsync_file_us: store.fsync_file_us,
            fsync_dir: store.fsync_dir,
            fsync_dir_us: store.fsync_dir_us,
            barrier_fs: store.barrier_fs,
            barrier_fs_us: store.barrier_fs_us,
            barrier_fs_batches: store.barrier_fs_batches,
            barrier_us: store.barrier_us(),
            put_bytes: store.put_bytes,
            dedup_bytes: store.dedup_bytes,
            get_bytes: store.get_bytes,
            hash_failures: store.hash_failures,
        }
    }
}

impl CacheCounterReport {
    pub fn of(cache: StoreCacheStats) -> Self {
        Self {
            object_cache_hits: cache.object_cache_hits,
            object_cache_misses: cache.object_cache_misses,
            tree_cache_hits: cache.tree_cache_hits,
            tree_cache_misses: cache.tree_cache_misses,
        }
    }
}

impl MetaCounterReport {
    pub fn of(meta: MetaStats) -> Self {
        Self {
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
        }
    }
}

impl ApiCounterReport {
    pub fn of(api: ApiStats) -> Self {
        Self {
            sessions_opened: api.sessions_opened,
            stale_observation: api.stale_observation,
            merge_applied: api.merge_applied,
            merge_conflict: api.merge_conflict,
            merge_base_us: api.merge_base_us,
            merge_base_searches: api.merge_base_searches,
            renames: api.renames,
            gc_runs: api.gc_runs,
            gc_objects_deleted: api.gc_objects_deleted,
            gc_bytes_deleted: api.gc_bytes_deleted,
            fsck_runs: api.fsck_runs,
            fsck_findings: api.fsck_findings,
        }
    }
}

/// The stable line label each counter section is rendered under. `forge bench`
/// renders sections independently -- a partial report has only some of them --
/// so the labels live here rather than in either renderer.
pub const STORE_LABEL: &str = "storage lifetime";
pub const CACHE_LABEL: &str = "cache lifetime  ";
pub const SQLITE_LABEL: &str = "sqlite lifetime ";
pub const API_LABEL: &str = "api lifetime    ";

impl StatsReport {
    pub(crate) fn build(
        store: BlobStoreStats,
        cache: StoreCacheStats,
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
            store: StoreCounterReport::of(store),
            cache: CacheCounterReport::of(cache),
            sqlite: MetaCounterReport::of(meta),
            api: ApiCounterReport::of(api),
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
        let mut out = format!(
            "forge stats schema={} scope={}\n{}\n\
             durability       journal_mode={} synchronous={} fullfsync={} read_only={}\n",
            self.schema_version,
            self.scope,
            self.note,
            self.durability.journal_mode,
            self.durability.synchronous,
            fullfsync,
            self.durability.read_only,
        );
        out.push_str(&self.counter_lines());
        out
    }

    /// The counter half of the human rendering, shared with `forge bench`.
    ///
    /// Derived from the same serialization the JSON document is, so every
    /// counter that exists in `forge stats --json` appears here BY
    /// CONSTRUCTION. That is the point, and it is the fix for the class of
    /// defect #324 reported: the previous renderer was a hand-written format
    /// string per section, `forge bench` had a second one, and the two
    /// silently disagreed -- bench omitted `cas_noop`, `dedup_hits`,
    /// `sessions_opened`, `merge_applied` and, the one that mattered, the
    /// write/read lock split, so a bench run could not attribute contention to
    /// the writer and reported it as `unavailable`. Adding a field to a
    /// counter report now cannot leave either surface behind.
    ///
    /// Line LABELS are stable and parsed by `scripts/w7_git_worktree_bench.py`;
    /// the key set inside a line is the JSON key set and grows with it.
    pub fn counter_lines(&self) -> String {
        let mut out = String::new();
        out.push_str(&counter_line(STORE_LABEL, &self.store));
        out.push_str(&counter_line(CACHE_LABEL, &self.cache));
        out.push_str(&counter_line(SQLITE_LABEL, &self.sqlite));
        out.push_str(&counter_line(API_LABEL, &self.api));
        out
    }
}

/// One `label key=value ...` line, with the keys taken from the section's own
/// serialization rather than written out again by hand.
pub fn counter_line(label: &str, section: &impl Serialize) -> String {
    let value = serde_json::to_value(section).expect("a counter section is plain integers");
    let fields = value
        .as_object()
        .expect("a counter section serializes to a JSON object");
    let mut line = String::from(label);
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&value.to_string());
    }
    line.push('\n');
    line
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
            self.store.cache_stats(),
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
                barrier_fs: 2,
                barrier_fs_us: 31,
                barrier_fs_batches: 5,
                put_bytes: 64,
                dedup_bytes: 32,
                get_bytes: 128,
                hash_failures: 0,
            },
            StoreCacheStats {
                object_cache_hits: 6,
                object_cache_misses: 2,
                tree_cache_hits: 9,
                tree_cache_misses: 4,
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
                merge_base_us: 12,
                merge_base_searches: 4,
                renames: 1,
                gc_runs: 1,
                gc_objects_deleted: 3,
                gc_bytes_deleted: 96,
                fsck_runs: 2,
                fsck_findings: 0,
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
        assert_eq!(report.store.barrier_us, 11 + 13 + 31);
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

    /// The rendering is DERIVED from the document, so this is a structural
    /// statement and not a list to keep in sync: every counter key the JSON
    /// carries appears in the text, and adding a field to a counter report
    /// cannot leave the human surface behind (#324).
    #[test]
    fn every_counter_key_in_the_document_appears_in_the_rendering() {
        let report = sample();
        let rendered = report.render();
        let doc = serde_json::to_value(&report).expect("the document serializes");
        for section in ["store", "cache", "sqlite", "api"] {
            let fields = doc[section]
                .as_object()
                .unwrap_or_else(|| panic!("{section} is an object"));
            assert!(!fields.is_empty(), "{section} carries no counters");
            for key in fields.keys() {
                assert!(
                    rendered.contains(&format!("{key}=")),
                    "render omits {section}.{key}:\n{rendered}"
                );
            }
        }
    }

    /// #324 by name: writer contention must be attributable, so the split must
    /// be present and must not be replaced by the sum.
    #[test]
    fn the_write_and_read_lock_halves_are_never_flattened_into_the_sum() {
        let rendered = sample().render();
        assert!(rendered.contains("write_lock_wait_us=15"), "{rendered}");
        assert!(rendered.contains("write_lock_acquires=20"), "{rendered}");
        assert!(rendered.contains("read_lock_wait_us=4"), "{rendered}");
        assert!(rendered.contains("read_lock_acquires=3"), "{rendered}");
        assert!(
            rendered.contains("lock_wait_us=19"),
            "the sum stays beside the split: {rendered}"
        );
    }
}
