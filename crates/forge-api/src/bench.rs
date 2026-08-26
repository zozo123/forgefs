//! Timed e2e workloads. Correctness first; numbers second.
//!
//! Where wall time actually goes on a local SSD:
//!   blob put  ≈ write + fsync(file) + hardlink + fsync(dir)   [I4, ~ms]
//!   tree COW  ≈ one new tree object per dirtied directory      [same]
//!   CAS       ≈ SQLite BEGIN IMMEDIATE + one UPDATE            [µs–ms]
//!   seal      ≈ type-aware hash walk of every reachable object [O(n)]
//!
//! Private refs do not contend on `main`. Shared-ref stampedes become forks
//! (I5/I8), not a lock convoy. SQLite still serializes the metadata txn.

use crate::{ApiStats, Forge};
use forge_cap::Cap;
use forge_store::{blob::BlobStoreStats, DurabilityPolicy, MetaStats, StoreCacheStats};
use forge_types::{CasResult, Error, Result};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Percentiles {
    pub n: usize,
    pub p50_us: u128,
    pub p95_us: u128,
    pub p99_us: u128,
    pub max_us: u128,
    pub sum_us: u128,
}

impl Percentiles {
    pub fn from_us(mut samples: Vec<u128>) -> Self {
        samples.sort_unstable();
        let n = samples.len();
        if n == 0 {
            return Self {
                n: 0,
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
                max_us: 0,
                sum_us: 0,
            };
        }
        let idx = |p: f64| ((n - 1) as f64 * p).round() as usize;
        Self {
            n,
            p50_us: samples[idx(0.50)],
            p95_us: samples[idx(0.95)],
            p99_us: samples[idx(0.99)],
            max_us: *samples.last().unwrap(),
            sum_us: samples.iter().sum(),
        }
    }

    pub fn throughput_hz(&self, wall: Duration) -> f64 {
        if wall.as_secs_f64() == 0.0 {
            return 0.0;
        }
        self.n as f64 / wall.as_secs_f64()
    }
}

#[derive(Clone, Debug)]
pub struct BenchReport {
    pub serial: Option<Percentiles>,
    pub private: Option<(Percentiles, Duration, usize)>,
    pub shared: Option<(Percentiles, Duration, usize, usize)>,
    /// Per-READ latencies, wall time, and how many logical readers produced
    /// them. The one phase in this harness whose catalog traffic lands mostly
    /// on the read pool rather than the write mutex, so the two halves of the
    /// lock split have something to disagree about (#324).
    pub read_fanout: Option<(Percentiles, Duration, usize)>,
    pub merge_seal: Option<Duration>,
    pub verify: Option<Duration>,
    pub durability: Option<DurabilityPolicy>,
    /// Process-lifetime snapshot, not a delta for any one benchmark phase.
    pub store: Option<BlobStoreStats>,
    /// Process-lifetime snapshot, not a delta for any one benchmark phase.
    pub cache: Option<StoreCacheStats>,
    /// Process-lifetime snapshot, not a delta for any one benchmark phase.
    pub meta: Option<MetaStats>,
    /// Process-lifetime snapshot, not a delta for any one benchmark phase.
    pub api: Option<ApiStats>,
}

impl BenchReport {
    pub fn render(&self) -> String {
        let mut s = String::from(
            "ForgeFS e2e bench (durable puts: fsync file+dir)\n\
             serial = one agent at a time (true op latency).\n\
             private = N threads, private refs (throughput; p50 includes convoy wait).\n\
             shared  = N threads, one ref; I8 pin ⇒ 1 Updated + N-1 Forked.\n\
             counter scope = cumulative whole-run process lifetime, never per-checkin.\n\
             lock split = write connection mutex vs read-pool slot mutexes; lock_acquires/lock_wait_us are their SUM and attribute nothing.\n\
             counter start = storage at blob-store construction; sqlite/api post-open.\n\
             counter end   = after init + all workloads + merge/seal + verify (+ worker fsck).\n\
             per-checkin mix = unavailable; requires operation-scoped tracing; never derive it from lifetime totals.\n",
        );
        if let Some(p) = &self.serial {
            s.push_str(&format!(
                "serial checkin   n={}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms\n",
                p.n,
                p.p50_us as f64 / 1000.0,
                p.p95_us as f64 / 1000.0,
                p.p99_us as f64 / 1000.0,
            ));
        }
        if let Some((p, wall, ok)) = &self.private {
            s.push_str(&format!(
                "private checkin  n={ok}/{}  wall={:.3}s  {:>7.1} Hz\n  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms\n",
                p.n,
                wall.as_secs_f64(),
                p.throughput_hz(*wall),
                p.p50_us as f64 / 1000.0,
                p.p95_us as f64 / 1000.0,
                p.p99_us as f64 / 1000.0,
                p.max_us as f64 / 1000.0,
            ));
        }
        if let Some((p, wall, updated, forked)) = &self.shared {
            s.push_str(&format!(
                "shared stampede  n={}  wall={:.3}s  updated={updated} forked={forked}\n  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms\n",
                p.n,
                wall.as_secs_f64(),
                p.p50_us as f64 / 1000.0,
                p.p95_us as f64 / 1000.0,
                p.p99_us as f64 / 1000.0,
            ));
        }
        if let Some((p, wall, readers)) = &self.read_fanout {
            s.push_str(&format!(
                "read fanout      readers={readers}  n={}  wall={:.3}s  {:>7.1} Hz\n  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms\n",
                p.n,
                wall.as_secs_f64(),
                p.throughput_hz(*wall),
                p.p50_us as f64 / 1000.0,
                p.p95_us as f64 / 1000.0,
                p.p99_us as f64 / 1000.0,
                p.max_us as f64 / 1000.0,
            ));
        }
        if let Some(d) = self.merge_seal {
            s.push_str(&format!("merge+seal       wall={:.3}s\n", d.as_secs_f64()));
        }
        if let Some(d) = self.verify {
            s.push_str(&format!("verify           wall={:.3}s\n", d.as_secs_f64()));
        }
        if let Some(policy) = &self.durability {
            let fullfsync = match policy.fullfsync {
                Some(true) => "on",
                Some(false) => "off",
                None => "n/a",
            };
            s.push_str(&format!(
                "durability       journal_mode={} synchronous=FULL({}) fullfsync={}\n",
                policy.journal_mode, policy.synchronous, fullfsync
            ));
        }
        // Counter lines, DERIVED from the same documents `forge stats --json`
        // emits rather than from a second format string of this renderer's own
        // (#42). A counter therefore cannot exist there and be missing here,
        // which is the class of omission #324 was one instance of.
        //
        // Rendered section by section, not as a whole `StatsReport`: a bench
        // report may hold only some of the four snapshots, and requiring all
        // four would have hidden the SQLite counters from exactly the partial
        // reports that exist to isolate them.
        if let Some(store) = self.store {
            s.push_str(&crate::stats::counter_line(
                crate::stats::STORE_LABEL,
                &crate::StoreCounterReport::of(store),
            ));
        }
        if let Some(cache) = self.cache {
            s.push_str(&crate::stats::counter_line(
                crate::stats::CACHE_LABEL,
                &crate::CacheCounterReport::of(cache),
            ));
        }
        if let Some(meta) = self.meta {
            s.push_str(&crate::stats::counter_line(
                crate::stats::SQLITE_LABEL,
                &crate::MetaCounterReport::of(meta),
            ));
            // The ATTRIBUTION line (#324). `lock_acquires` / `lock_wait_us`
            // have summed the write connection's mutex and the read pool since
            // #315, so a writer convoy and a busy read pool render
            // identically. Every performance conclusion in this project has
            // come from these counters, and `lock_wait_us` alone has already
            // produced three wrong ones on #37. The split is printed beside
            // the sum, not instead of it, so the arithmetic stays checkable by
            // eye -- and `write_share_of_wait` is a derived ratio, so the
            // counter document has nowhere to carry it and this line stays.
            s.push_str(&format!(
                "sqlite locks     write_acquires={} write_wait_us={}  read_acquires={} read_wait_us={}  write_share_of_wait={}\n",
                meta.write_lock_acquires,
                meta.write_lock_wait_us,
                meta.read_lock_acquires,
                meta.read_lock_wait_us,
                share(meta.write_lock_wait_us, meta.lock_wait_us),
            ));
        }
        if let Some(api) = self.api {
            s.push_str(&crate::stats::counter_line(
                crate::stats::API_LABEL,
                &crate::ApiCounterReport::of(api),
            ));
        }
        if let (Some(store), Some(meta)) = (self.store, self.meta) {
            let cumulative_phase_us = store
                .barrier_us()
                .saturating_add(meta.sqlite_accounted_us());
            s.push_str(&format!(
                "lifetime phases  fsync_file_us={} + fsync_dir_us={} + barrier_fs_us={} + sqlite_lock_wait_us={} + sqlite_txn_us={} = cumulative_phase_us={}\n",
                store.fsync_file_us,
                store.fsync_dir_us,
                store.barrier_fs_us,
                meta.lock_wait_us,
                meta.txn_us,
                cumulative_phase_us,
            ));
        }
        s
    }
}

fn us(d: Duration) -> u128 {
    d.as_micros()
}

/// `part` as a percentage of `whole`, or the literal `n/a` when nothing waited.
///
/// A zero denominator is not 0% and not 100%: no lock wait was recorded at all,
/// so the split has nothing to attribute and must say so rather than print a
/// number a reader would take for a measurement.
fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".into();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

fn one_private(forge: &Forge, root: &Cap, i: usize) -> Result<CasResult> {
    let agent = forge.grant(
        root,
        vec![
            "ops=read,write,branch".into(),
            format!("agent=ser{i}"),
            "ref=heads/agents/*,main".into(),
        ],
    )?;
    let ns = forge.session_open(&agent, "main")?;
    forge.write(
        &agent,
        &ns,
        &format!("/ser{i}.txt"),
        format!("serial {i}").as_bytes(),
        false,
    )?;
    forge.checkin(&agent, &ns, "/", "serial")
}

/// Sequential baseline: true single-agent checkin latency (grant+session+write+CAS).
pub fn serial_checkins(forge: &Forge, root: &Cap, n: usize) -> Result<Percentiles> {
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let t0 = Instant::now();
        let r = one_private(forge, root, i)?;
        if !matches!(r, CasResult::Updated { .. }) {
            return Err(Error::Internal(format!(
                "serial checkin not Updated: {r:?}"
            )));
        }
        samples.push(us(t0.elapsed()));
    }
    Ok(Percentiles::from_us(samples))
}

/// N agents, private live refs, unique files. All checkins should `Updated`.
pub fn private_checkins(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
) -> Result<(Percentiles, Duration, usize)> {
    let mut handles = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        let f = forge.clone();
        let cap = root.clone();
        handles.push(thread::spawn(move || -> Result<(u128, CasResult)> {
            let t0 = Instant::now();
            let agent = f.grant(
                &cap,
                vec![
                    "ops=read,write,branch".into(),
                    format!("agent=bench{i}"),
                    "ref=heads/agents/*,main".into(),
                ],
            )?;
            let ns = f.session_open(&agent, "main")?;
            f.write(
                &agent,
                &ns,
                &format!("/w{i}.txt"),
                format!("agent {i}").as_bytes(),
                false,
            )?;
            let r = f.checkin(&agent, &ns, "/", "bench")?;
            Ok((us(t0.elapsed()), r))
        }));
    }
    let mut samples = Vec::new();
    let mut ok = 0usize;
    for h in handles {
        let (dt, r) = h
            .join()
            .map_err(|_| Error::Internal("worker panic".into()))??;
        samples.push(dt);
        if matches!(r, CasResult::Updated { .. }) {
            ok += 1;
        }
    }
    Ok((Percentiles::from_us(samples), start.elapsed(), ok))
}

/// N agents remount the same shared ref. Snapshot pinning ⇒ 1 Updated, N-1 Forked.
pub fn shared_stampede(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
) -> Result<(Percentiles, Duration, usize, usize)> {
    forge.branch(root, "main", "shared")?;
    let mut prepared = Vec::with_capacity(n);
    for i in 0..n {
        let agent = forge.grant(
            root,
            vec![
                "ops=read,write,branch".into(),
                format!("agent=share{i}"),
                "ref=heads/agents/*,main,shared,forks/*".into(),
            ],
        )?;
        let ns = forge.session_open(&agent, "shared")?;
        forge.mount(&agent, &ns, "/", "ref:shared", true)?;
        forge.write(
            &agent,
            &ns,
            &format!("/s{i}.txt"),
            format!("{i}").as_bytes(),
            false,
        )?;
        prepared.push((agent, ns));
    }
    let start = Instant::now();
    let mut handles = Vec::new();
    for (agent, ns) in prepared {
        let f = forge.clone();
        handles.push(thread::spawn(move || -> Result<(u128, CasResult)> {
            let t0 = Instant::now();
            let r = f.checkin(&agent, &ns, "/", "shared")?;
            Ok((us(t0.elapsed()), r))
        }));
    }
    let mut samples = Vec::new();
    let mut updated = 0usize;
    let mut forked = 0usize;
    for h in handles {
        let (dt, r) = h
            .join()
            .map_err(|_| Error::Internal("worker panic".into()))??;
        samples.push(dt);
        match r {
            CasResult::Updated { .. } => updated += 1,
            CasResult::Forked { .. } => forked += 1,
            CasResult::Noop { .. } => {}
        }
    }
    Ok((
        Percentiles::from_us(samples),
        start.elapsed(),
        updated,
        forked,
    ))
}

/// Integrator folds every `heads/agents/*` into main, then seals.
pub fn merge_all_and_seal(forge: &Forge, root: &Cap, integ: &Cap, tag: &str) -> Result<Duration> {
    let t0 = Instant::now();
    let refs = forge.refs(root)?;
    for r in refs {
        if !r.name.starts_with("heads/agents/") {
            continue;
        }
        match forge.merge(integ, "main", &r.name, None) {
            Ok(_) | Err(Error::MergeConflict(_)) => {}
            Err(e) => return Err(e),
        }
    }
    forge.seal(integ, "main", tag)?;
    Ok(t0.elapsed())
}

pub fn run(dir: &std::path::Path, agents: usize, shared: usize) -> Result<BenchReport> {
    let forge = Arc::new(Forge::init(dir)?);
    let root = forge.root_cap()?;
    let integ = forge.integrator_cap()?;
    let serial = serial_checkins(&forge, &root, 8.min(agents))?;
    let private = private_checkins(forge.clone(), &root, agents)?;
    let shared_r = shared_stampede(forge.clone(), &root, shared)?;
    let merge_seal = merge_all_and_seal(&forge, &root, &integ, "bench")?;
    let t0 = Instant::now();
    forge.verify_tag(&root, "bench")?;
    let verify = t0.elapsed();
    let store = forge.store.stats();
    Ok(BenchReport {
        serial: Some(serial),
        private: Some(private),
        shared: Some(shared_r),
        // The in-process API entry point stays write-only: the read-heavy
        // phase is driven by `run_bench_with_workers`, which is what `forge
        // bench` calls and what carries the `--readers` knob (#324).
        read_fanout: None,
        merge_seal: Some(merge_seal),
        verify: Some(verify),
        durability: Some(forge.store.meta.durability_policy().clone()),
        store: Some(store),
        cache: Some(forge.store.cache_stats()),
        meta: Some(forge.store.meta.stats()),
        api: Some(forge.api_stats()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench renderer must emit the SAME counter set `forge stats` does.
    ///
    /// #324: it did not. It rendered only the summed `lock_wait_us`, so a
    /// bench run could not say whether the writer or the read pool waited, and
    /// a storage sweep had to report writer contention as `unavailable`. It
    /// also silently dropped `cas_noop`, `dedup_hits`, `sessions_opened` and
    /// `merge_applied`. Both surfaces now derive their lines from the counter
    /// documents, and this test is what keeps the two from drifting again.
    #[test]
    fn render_emits_every_counter_the_stats_document_carries() {
        let store = BlobStoreStats {
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
        };
        let cache = StoreCacheStats {
            object_cache_hits: 6,
            object_cache_misses: 2,
            tree_cache_hits: 9,
            tree_cache_misses: 4,
        };
        let meta = MetaStats {
            txn_us: 17,
            txn_count: 9,
            explicit_txn_count: 5,
            lock_wait_us: 19,
            lock_acquires: 23,
            write_lock_acquires: 20,
            write_lock_wait_us: 15,
            read_lock_acquires: 3,
            read_lock_wait_us: 4,
            busy: 0,
            cas_updated: 7,
            cas_forked: 1,
            cas_denied: 0,
            cas_noop: 0,
        };
        let api = ApiStats {
            stale_observation: 2,
            merge_conflict: 3,
            sessions_opened: 0,
            merge_applied: 0,
            merge_base_us: 12,
            merge_base_searches: 4,
            renames: 1,
            gc_runs: 1,
            gc_objects_deleted: 3,
            gc_bytes_deleted: 96,
            fsck_runs: 2,
            fsck_findings: 0,
        };
        let durability = DurabilityPolicy {
            journal_mode: "wal".into(),
            synchronous: 2,
            fullfsync: None,
            read_only: false,
        };
        let report = BenchReport {
            serial: None,
            private: None,
            shared: None,
            read_fanout: None,
            merge_seal: None,
            verify: None,
            durability: Some(durability.clone()),
            store: Some(store),
            cache: Some(cache),
            meta: Some(meta),
            api: Some(api),
        };

        let rendered = report.render();
        assert!(rendered
            .contains("counter scope = cumulative whole-run process lifetime, never per-checkin"));
        assert!(rendered
            .contains("counter start = storage at blob-store construction; sqlite/api post-open"));
        assert!(rendered.contains(
            "counter end   = after init + all workloads + merge/seal + verify (+ worker fsck)"
        ));
        assert!(rendered.contains(
            "per-checkin mix = unavailable; requires operation-scoped tracing; never derive it from lifetime totals"
        ));
        assert!(rendered.contains(
            "fsync_file_us=11 + fsync_dir_us=13 + barrier_fs_us=31 + sqlite_lock_wait_us=19 + sqlite_txn_us=17 = cumulative_phase_us=91"
        ));
        assert!(rendered.contains(
            "per-checkin mix = unavailable; requires operation-scoped tracing; never derive it from lifetime totals"
        ));
        // #324: the split is rendered beside the sum, and the sum is still
        // there -- a reader must be able to check 20+3=23 and 15+4=19 by eye.
        assert!(rendered.contains(
            "sqlite locks     write_acquires=20 write_wait_us=15  read_acquires=3 read_wait_us=4  write_share_of_wait=78.9%"
        ));
        assert!(rendered.contains(
            "lock split = write connection mutex vs read-pool slot mutexes; lock_acquires/lock_wait_us are their SUM and attribute nothing"
        ));

        // #42: the counter lines are DERIVED from the document, so this is a
        // structural statement rather than a list to keep in sync. It replaces
        // an assertion on the literal string `api lifetime     stale=2
        // conflict=3`, which pinned an abbreviation the document does not use
        // -- and pinning abbreviations per section is how the two surfaces
        // drifted apart in the first place.
        for (section, keys) in [
            (
                "store",
                serde_json::to_value(crate::StoreCounterReport::of(store)).unwrap(),
            ),
            (
                "cache",
                serde_json::to_value(crate::CacheCounterReport::of(cache)).unwrap(),
            ),
            (
                "sqlite",
                serde_json::to_value(crate::MetaCounterReport::of(meta)).unwrap(),
            ),
            (
                "api",
                serde_json::to_value(crate::ApiCounterReport::of(api)).unwrap(),
            ),
        ] {
            for key in keys.as_object().unwrap().keys() {
                assert!(
                    rendered.contains(&format!("{key}=")),
                    "bench render omits {section}.{key}, which `forge stats --json` reports:\n{rendered}"
                );
            }
        }

        assert!(!rendered.contains("observed mix"));
        assert!(!rendered.contains("observed_us"));
        assert!(!rendered.contains("bytes=unavailable"));
    }

    fn only_meta(meta: MetaStats) -> BenchReport {
        BenchReport {
            serial: None,
            private: None,
            shared: None,
            read_fanout: None,
            merge_seal: None,
            verify: None,
            durability: None,
            store: None,
            cache: None,
            meta: Some(meta),
            api: None,
        }
    }

    fn line<'a>(rendered: &'a str, prefix: &str) -> &'a str {
        rendered
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} line in:\n{rendered}"))
    }

    /// #324. Two runs whose SUMMED lock counters are identical, one a writer
    /// convoy and one a busy read pool. The renderer printed only the sum, so
    /// they produced byte-identical output and no reader could tell a queue on
    /// the write connection from a queue on the read pool -- which is the
    /// counter every performance conclusion in this project has come from, and
    /// which `lock_wait_us` alone has already produced three wrong ones with.
    #[test]
    fn the_lock_split_distinguishes_a_writer_convoy_from_a_busy_read_pool() {
        let convoy = MetaStats {
            lock_acquires: 1000,
            lock_wait_us: 1000,
            write_lock_acquires: 90,
            write_lock_wait_us: 970,
            read_lock_acquires: 910,
            read_lock_wait_us: 30,
            ..MetaStats::default()
        };
        let pool = MetaStats {
            write_lock_wait_us: 30,
            read_lock_wait_us: 970,
            ..convoy
        };
        // The premise: everything the old renderer printed is identical.
        assert_eq!(convoy.lock_acquires, pool.lock_acquires);
        assert_eq!(convoy.lock_wait_us, pool.lock_wait_us);
        assert_eq!(convoy.sqlite_accounted_us(), pool.sqlite_accounted_us());

        let a = only_meta(convoy).render();
        let b = only_meta(pool).render();

        // The SUMMED FIELDS are what cannot tell these apart, and that is the
        // statement this test has always been making. It used to be checkable
        // as "the whole `sqlite lifetime` line is byte-identical", because that
        // line rendered only the sums. Since #42 the line is derived from the
        // counter document, so it carries the split too and the two runs differ --
        // the property is unchanged and the instrument is strictly better, but
        // the assertion had to move from the LINE to the FIELDS it is about.
        for field in [
            "lock_acquires=1000",
            "lock_wait_us=1000",
            "accounted_us=1000",
        ] {
            assert!(
                line(&a, "sqlite lifetime").contains(field)
                    && line(&b, "sqlite lifetime").contains(field),
                "the summed fields are identical for both runs: {field}"
            );
        }
        assert_ne!(
            line(&a, "sqlite locks"),
            line(&b, "sqlite locks"),
            "the split must tell them apart"
        );
        assert!(
            line(&a, "sqlite locks").contains("write_share_of_wait=97.0%"),
            "{}",
            line(&a, "sqlite locks")
        );
        assert!(
            line(&b, "sqlite locks").contains("write_share_of_wait=3.0%"),
            "{}",
            line(&b, "sqlite locks")
        );
    }

    /// A run that never waited has no split to report. Printing `0.0%` would
    /// read as "no writer contention was measured", which is a claim; `n/a` is
    /// the absence of one.
    #[test]
    fn a_run_that_never_waited_reports_no_share_rather_than_zero_percent() {
        let rendered = only_meta(MetaStats {
            lock_acquires: 4,
            ..MetaStats::default()
        })
        .render();
        assert!(
            line(&rendered, "sqlite locks").ends_with("write_share_of_wait=n/a"),
            "{}",
            line(&rendered, "sqlite locks")
        );
    }

    /// The read-heavy phase is rendered, and its label says how many logical
    /// readers produced the samples -- `n` alone is readers x reads and cannot
    /// be read as a concurrency point.
    #[test]
    fn the_read_phase_names_its_reader_count() {
        let rendered = BenchReport {
            read_fanout: Some((
                Percentiles::from_us(vec![10, 20, 30, 40]),
                Duration::from_millis(500),
                7,
            )),
            ..only_meta(MetaStats::default())
        }
        .render();
        assert!(
            rendered.contains("read fanout      readers=7  n=4  wall=0.500s"),
            "{rendered}"
        );
    }
}
