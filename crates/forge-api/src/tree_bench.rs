//! W6: large-tree walk (lookup) and update scaling study.
//!
//! Correctness: every entry reads the expected payload; the exported tree matches
//! a deterministic golden (byte-for-byte across fresh repositories with the same
//! entry count). Do not infer W6 from W1/W2.

use crate::bench::Percentiles;
use crate::Forge;
use forge_cap::Cap;
use forge_core::hash_bytes;
use forge_types::{CasResult, Error, Result};
use std::path::Path;
use std::time::{Duration, Instant};

const FILES_PER_DIR: usize = 1000;

/// Two-level paths keep each directory under `MAX_TREE_ENTRIES` up to 1M entries.
pub fn w6_path(index: usize) -> String {
    let dir = index / FILES_PER_DIR;
    let file = index % FILES_PER_DIR;
    format!("/d{dir:04}/f{file:04}.txt")
}

pub fn w6_seed_payload(index: usize) -> Vec<u8> {
    format!("w6 entry {index} seed v0").into_bytes()
}

pub fn w6_update_payload(index: usize) -> Vec<u8> {
    format!("w6 entry {index} update v1").into_bytes()
}

#[derive(Clone, Debug)]
pub struct TreeBenchReport {
    pub entries: usize,
    pub populate_wall: Duration,
    pub lookup: (Percentiles, Duration),
    pub update: (Percentiles, Duration),
    pub checkin_wall: Duration,
    /// BLAKE3 of the exported tar after the final checkin (deterministic tree).
    pub export_blake3: String,
    pub fsck_ok: bool,
    pub durability: Option<forge_store::DurabilityPolicy>,
    pub store: Option<forge_store::blob::BlobStoreStats>,
    pub cache: Option<forge_store::StoreCacheStats>,
    pub meta: Option<forge_store::MetaStats>,
    pub api: Option<crate::ApiStats>,
}

impl TreeBenchReport {
    pub fn render(&self) -> String {
        let (lookup_p, lookup_wall) = &self.lookup;
        let (update_p, update_wall) = &self.update;
        let mut s = format!(
            "ForgeFS W6 tree walk/update (entries={})\n\
             correctness: every lookup/update payload matches; export tar is deterministic.\n",
            self.entries
        );
        s.push_str(&format!(
            "populate         wall={:.3}s\n",
            self.populate_wall.as_secs_f64()
        ));
        s.push_str(&format!(
            "lookup           n={}  wall={:.3}s  {:>7.1} Hz\n  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms\n",
            lookup_p.n,
            lookup_wall.as_secs_f64(),
            lookup_p.throughput_hz(*lookup_wall),
            lookup_p.p50_us as f64 / 1000.0,
            lookup_p.p95_us as f64 / 1000.0,
            lookup_p.p99_us as f64 / 1000.0,
            lookup_p.max_us as f64 / 1000.0,
        ));
        s.push_str(&format!(
            "update           n={}  wall={:.3}s  {:>7.1} Hz\n  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms\n",
            update_p.n,
            update_wall.as_secs_f64(),
            update_p.throughput_hz(*update_wall),
            update_p.p50_us as f64 / 1000.0,
            update_p.p95_us as f64 / 1000.0,
            update_p.p99_us as f64 / 1000.0,
            update_p.max_us as f64 / 1000.0,
        ));
        s.push_str(&format!(
            "checkin          wall={:.3}s\n",
            self.checkin_wall.as_secs_f64()
        ));
        s.push_str(&format!(
            "export identity  blake3={}  fsck={}\n",
            self.export_blake3,
            if self.fsck_ok { "clean" } else { "FAILED" }
        ));
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
        }
        if let Some(api) = self.api {
            s.push_str(&crate::stats::counter_line(
                crate::stats::API_LABEL,
                &crate::ApiCounterReport::of(api),
            ));
        }
        s
    }
}

fn us(d: Duration) -> u128 {
    d.as_micros()
}

fn verify_reads(
    forge: &Forge,
    cap: &Cap,
    ns: &str,
    entries: usize,
    payload: fn(usize) -> Vec<u8>,
) -> Result<()> {
    for i in 0..entries {
        let path = w6_path(i);
        let got = forge.read(cap, ns, &path)?;
        let want = payload(i);
        if got != want {
            return Err(Error::Internal(format!(
                "W6 lookup mismatch at {path}: expected {} bytes, got {}",
                want.len(),
                got.len()
            )));
        }
    }
    Ok(())
}

fn export_tar_digest(forge: &Forge, cap: &Cap) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("forge-w6-export-{}.tar", ulid::Ulid::new()));
    forge.export_tar(cap, "main", &tmp)?;
    let bytes = std::fs::read(&tmp).map_err(|e| Error::Io(e.to_string()))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(hash_bytes(&bytes).hex())
}

/// Run the W6 harness on a fresh repository at `dir` with `entries` leaf files.
pub fn run_tree_bench(dir: &Path, entries: usize) -> Result<TreeBenchReport> {
    if entries == 0 {
        return Err(Error::Invalid("W6 requires at least one tree entry".into()));
    }
    let forge = Forge::init(dir)?;
    let root = forge.root_cap()?;
    let ns = forge.session_open(&root, "main")?;

    let populate_start = Instant::now();
    for i in 0..entries {
        forge.write(&root, &ns, &w6_path(i), &w6_seed_payload(i), false)?;
    }
    match forge.checkin(&root, &ns, "/", "w6 seed")? {
        CasResult::Updated { .. } => {}
        other => {
            return Err(Error::Internal(format!(
                "W6 seed checkin did not publish: {other:?}"
            )))
        }
    }
    let populate_wall = populate_start.elapsed();

    verify_reads(&forge, &root, &ns, entries, w6_seed_payload)?;

    let lookup_start = Instant::now();
    let mut lookup_samples = Vec::with_capacity(entries);
    for i in 0..entries {
        let t0 = Instant::now();
        let got = forge.read(&root, &ns, &w6_path(i))?;
        lookup_samples.push(us(t0.elapsed()));
        if got != w6_seed_payload(i) {
            return Err(Error::Internal(format!(
                "W6 timed lookup mismatch at {}",
                w6_path(i)
            )));
        }
    }
    let lookup_wall = lookup_start.elapsed();
    let lookup = (Percentiles::from_us(lookup_samples), lookup_wall);

    let update_start = Instant::now();
    let mut update_samples = Vec::with_capacity(entries);
    for i in 0..entries {
        let t0 = Instant::now();
        forge.write(&root, &ns, &w6_path(i), &w6_update_payload(i), false)?;
        update_samples.push(us(t0.elapsed()));
    }
    let update_wall = update_start.elapsed();
    let update = (Percentiles::from_us(update_samples), update_wall);

    let checkin_start = Instant::now();
    match forge.checkin(&root, &ns, "/", "w6 update")? {
        CasResult::Updated { .. } => {}
        other => {
            return Err(Error::Internal(format!(
                "W6 update checkin did not publish: {other:?}"
            )))
        }
    }
    let checkin_wall = checkin_start.elapsed();

    verify_reads(&forge, &root, &ns, entries, w6_update_payload)?;
    let export_blake3 = export_tar_digest(&forge, &root)?;

    let fsck = forge.fsck(&root, true)?;
    if !fsck.ok {
        return Err(Error::Corrupt(format!(
            "W6 repository failed full fsck with {} finding(s)",
            fsck.findings.len()
        )));
    }

    Ok(TreeBenchReport {
        entries,
        populate_wall,
        lookup,
        update,
        checkin_wall,
        export_blake3,
        fsck_ok: fsck.ok,
        durability: Some(forge.store.meta.durability_policy().clone()),
        store: Some(forge.store.stats()),
        cache: Some(forge.store.cache_stats()),
        meta: Some(forge.store.meta.stats()),
        api: Some(forge.api_stats()),
    })
}
