//! Directory-barrier budget harness (#177).
//!
//! Ignored by default: this is a measurement instrument, not a gate. It runs
//! the W1 private-checkin shape at one worker count against a fresh
//! repository and reports, for the timed region only:
//!
//!   * wall throughput,
//!   * the object-store barrier counters delta (file and directory),
//!   * the SQLite committed-transaction delta,
//!   * the block device flush-request delta read from `/proc/diskstats`.
//!
//! The last field is the one that separates "we issue fewer barriers" from
//! "we got a faster number": it is what the hardware actually saw. Field 19
//! of a `/proc/diskstats` row is "flush requests completed" (Linux 4.18+).
//!
//! Deltas are taken around the timed region only, so unlike the process
//! lifetime counters in `forge stats` these ARE per-checkin measurements of
//! the workload -- `Forge::init`, capability-root setup and the final `fsck`
//! are all outside the window.
//!
//! ```text
//! FORGEFS_BB_DEV=vdd FORGEFS_BB_N=240 FORGEFS_BB_W=4 \
//!   FORGEFS_BB_DIR=/workspace/bb \
//!   cargo test --release -p forge-api --test dir_barrier_budget -- --ignored --nocapture
//! ```
use forge_api::{private_checkins_bounded, Forge};
use std::sync::Arc;

fn device_flushes(dev: &str) -> Option<u64> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    for line in stats.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 19 && f[2] == dev {
            return f[18].parse().ok();
        }
    }
    None
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[test]
#[ignore = "measurement harness; run explicitly with --ignored"]
fn dir_barrier_budget() {
    let n: usize = env_or("FORGEFS_BB_N", "240").parse().expect("FORGEFS_BB_N");
    let w: usize = env_or("FORGEFS_BB_W", "1").parse().expect("FORGEFS_BB_W");
    let dev = env_or("FORGEFS_BB_DEV", "");
    let label = env_or("FORGEFS_BB_LABEL", "run");
    let dir = std::env::var("FORGEFS_BB_DIR").expect("FORGEFS_BB_DIR must name a fresh directory");
    let dir = std::path::PathBuf::from(dir);
    assert!(!dir.exists(), "{} already exists", dir.display());
    std::fs::create_dir_all(&dir).unwrap();

    let f = Arc::new(Forge::init(&dir).unwrap());
    let root = f.root_cap().unwrap();

    let before = f.stats_report();
    let flush_before = if dev.is_empty() {
        None
    } else {
        device_flushes(&dev)
    };
    let (p, wall, updated) = private_checkins_bounded(f.clone(), &root, n, w).unwrap();
    let flush_after = if dev.is_empty() {
        None
    } else {
        device_flushes(&dev)
    };
    let after = f.stats_report();
    assert_eq!(updated, n, "every private checkin must be Updated");

    let nf = n as f64;
    let file = after.store.fsync_file - before.store.fsync_file;
    let dirs = after.store.fsync_dir - before.store.fsync_dir;
    let fsb = after.store.barrier_fs - before.store.barrier_fs;
    let fsb_batches = after.store.barrier_fs_batches - before.store.barrier_fs_batches;
    let txn = after.sqlite.txn_count - before.sqlite.txn_count;
    let puts = after.store.puts - before.store.puts;
    let dedup = after.store.dedup_hits - before.store.dedup_hits;
    let device = match (flush_before, flush_after) {
        (Some(a), Some(b)) => format!("{:.3}", (b - a) as f64 / nf),
        _ => "unavailable".to_string(),
    };
    println!(
        "BB label={label} w={w} n={n} wall={:.4}s hz={:.1} p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms \
puts_per={:.3} dedup_per={:.3} fsync_file_per={:.3} fsync_dir_per={:.3} barrier_fs_per={:.3} barrier_fs_batches_per={:.3} txn_per={:.3} \
forge_flush_per={:.3} device_flush_per={device}",
        wall.as_secs_f64(),
        p.throughput_hz(wall),
        p.p50_us as f64 / 1000.0,
        p.p95_us as f64 / 1000.0,
        p.p99_us as f64 / 1000.0,
        p.max_us as f64 / 1000.0,
        puts as f64 / nf,
        dedup as f64 / nf,
        file as f64 / nf,
        dirs as f64 / nf,
        fsb as f64 / nf,
        fsb_batches as f64 / nf,
        txn as f64 / nf,
        (file + dirs + fsb + txn) as f64 / nf,
    );

    let report = f.fsck(&root, true).unwrap();
    assert!(report.ok, "fsck --full must be clean: {report:?}");
}
