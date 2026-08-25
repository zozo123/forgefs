//! I19 under concurrency: collect in a loop while many sessions write, check
//! in, fork and abandon, then prove `fsck --full` finds nothing dangling.
//!
//! This is the test that decides whether collection is trustworthy. A
//! single-threaded "it deleted the right objects" test proves almost nothing
//! here, because the defect this whole design exists to prevent is a race:
//! `gc` decides X is unreachable, a session publishes a tree naming X, `gc`
//! unlinks X, and I4 is broken silently. Content addressing makes it *more*
//! likely, not less -- a writer that reproduces bytes X already holds does not
//! rewrite them (I3), so the writers below deliberately draw their payloads
//! from a small pool so that deduplicating puts are the common case rather
//! than a curiosity.
//!
//! `FORGE_GC_SOAK_SECS` lengthens the run; the default is the shortest run
//! that still lets objects age past `GC_COLLECT_MIN_AGE_FLOOR` and be
//! collected while writers are mid-flight.

use forge_api::{Forge, GC_COLLECT_MIN_AGE_FLOOR};
use forge_store::Store;
use forge_types::CasResult;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tempfile::tempdir;

const WRITERS: usize = 6;
const DEFAULT_SECS: u64 = 20;
/// How many distinct payloads exist as pre-aged, unreachable garbage before
/// the concurrent phase starts. Every writer draws from this pool, so nearly
/// every `write` in the run is a deduplicating put against an object the
/// collector is entitled to sweep -- which is the race, made the common case
/// instead of a curiosity.
const SEEDED_GARBAGE: usize = 4_000;

fn payload(n: usize) -> Vec<u8> {
    format!("soak payload {n}: bytes several agents will independently reproduce").into_bytes()
}

/// Make an object as old as a repository that has been running for an hour.
fn backdate(path: &Path, by: Duration) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - by).unwrap();
}

fn walk_objects(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let objects = root.join("objects");
    let Ok(shards) = fs::read_dir(&objects) else {
        return out;
    };
    for a in shards.flatten() {
        if !a.path().is_dir() {
            continue;
        }
        for b in fs::read_dir(a.path()).unwrap().flatten() {
            if !b.path().is_dir() {
                continue;
            }
            for file in fs::read_dir(b.path()).unwrap().flatten() {
                if file.path().is_file() {
                    out.push(file.path());
                }
            }
        }
    }
    out
}

fn soak_secs() -> u64 {
    std::env::var("FORGE_GC_SOAK_SECS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(DEFAULT_SECS)
}

#[test]
fn collection_never_dangles_a_reference_under_a_sustained_concurrent_load() {
    let dir = tempdir().unwrap();
    let forge = Arc::new(Forge::init(dir.path()).unwrap());
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "shared").unwrap();
    {
        let seed = forge.session_open(&root, "shared").unwrap();
        forge.mount(&root, &seed, "/", "ref:shared", true).unwrap();
        forge
            .write(&root, &seed, "/seed.txt", b"v0", false)
            .unwrap();
        forge.checkin(&root, &seed, "/", "seed").unwrap();
    }

    // Seed the repository with unreachable objects, then age them past the
    // floor. Backdating before the concurrent phase is not a shortcut around
    // the safety argument: it puts the repository into the state a real one
    // reaches after an hour of churn, and every object created *during* the
    // phase carries its true age and is protected by the real floor. What it
    // buys is that the collector has work to do from the first sweep instead
    // of from minute two, so the sweep and the writers actually overlap.
    {
        let seeder = Store::open(forge.root()).unwrap();
        for n in 0..SEEDED_GARBAGE {
            seeder.put_blob_data(&payload(n)).unwrap();
        }
    }
    let aged = walk_objects(forge.root());
    for path in &aged {
        backdate(path, Duration::from_secs(3_600));
    }
    eprintln!("gc soak: aged {} objects past the floor", aged.len());

    let deadline = Instant::now() + Duration::from_secs(soak_secs());
    let stop = Arc::new(AtomicBool::new(false));
    let checkins = Arc::new(AtomicU64::new(0));
    let forks = Arc::new(AtomicU64::new(0));
    let abandons = Arc::new(AtomicU64::new(0));
    let sweeps = Arc::new(AtomicU64::new(0));
    let unlinked = Arc::new(AtomicU64::new(0));
    // The floor is only a bound if no single put-to-publish interval exceeds
    // it, and that is a precondition the sweep cannot check for itself. The
    // load measures it, so the margin this run actually had is evidence and
    // not an assumption.
    let slowest_publish_us = Arc::new(AtomicU64::new(0));

    let mut threads = Vec::new();
    for writer in 0..WRITERS {
        let forge = Arc::clone(&forge);
        let root = root.clone();
        let stop = Arc::clone(&stop);
        let checkins = Arc::clone(&checkins);
        let forks = Arc::clone(&forks);
        let abandons = Arc::clone(&abandons);
        let slowest = Arc::clone(&slowest_publish_us);
        threads.push(std::thread::spawn(move || {
            let mut round = 0u64;
            while !stop.load(Ordering::Relaxed) {
                round += 1;
                let Ok(ns) = forge.session_open(&root, "shared") else {
                    continue;
                };
                if forge.mount(&root, &ns, "/", "ref:shared", true).is_err() {
                    continue;
                }
                // The put happens here; the transaction that publishes a root
                // naming it happens in `checkin`. That interval is the window
                // the age floor has to bound.
                let started = Instant::now();
                let bytes = payload((round as usize) * WRITERS + writer);
                let path = format!("/w{writer}-{}.txt", round % 5);
                if forge.write(&root, &ns, &path, &bytes, false).is_err() {
                    let _ = forge.abandon_session(&root, &ns, true);
                    continue;
                }
                match forge.checkin(&root, &ns, "/", "soak") {
                    Ok(CasResult::Updated { .. }) => {
                        checkins.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(CasResult::Forked { fork, .. }) => {
                        forks.fetch_add(1, Ordering::Relaxed);
                        // Retire most forks: an unresolved fork is a root, so
                        // a run that never abandons one produces no garbage
                        // for the collector to race against.
                        if !round.is_multiple_of(4) {
                            // Record before returning: a losing checkin is the
                            // slow path, so skipping it here would measure only
                            // the fast half of the distribution.
                            slowest
                                .fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                            let _ = forge.abandon_session(&root, &ns, true);
                            if forge.abandon_fork(&root, &fork).is_ok() {
                                abandons.fetch_add(1, Ordering::Relaxed);
                            }
                            continue;
                        }
                    }
                    Ok(_) | Err(_) => {
                        slowest.fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
                let elapsed = started.elapsed().as_micros() as u64;
                slowest.fetch_max(elapsed, Ordering::Relaxed);
                let _ = forge.abandon_session(&root, &ns, true);
            }
        }));
    }

    {
        let forge = Arc::clone(&forge);
        let root = root.clone();
        let stop = Arc::clone(&stop);
        let sweeps = Arc::clone(&sweeps);
        let unlinked = Arc::clone(&unlinked);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match forge.gc_collect(&root, GC_COLLECT_MIN_AGE_FLOOR) {
                    Ok(report) => {
                        sweeps.fetch_add(1, Ordering::Relaxed);
                        unlinked.fetch_add(report.deleted_objects as u64, Ordering::Relaxed);
                    }
                    // Contention on the catalog write lock is expected and is
                    // not a correctness event; anything else is.
                    Err(forge_types::Error::Busy(_)) => {}
                    Err(error) => panic!("collection failed: {error}"),
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }));
    }

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().unwrap();
    }

    let checkins = checkins.load(Ordering::Relaxed);
    let forks = forks.load(Ordering::Relaxed);
    let abandons = abandons.load(Ordering::Relaxed);
    let sweeps = sweeps.load(Ordering::Relaxed);
    let unlinked = unlinked.load(Ordering::Relaxed);
    let slowest_us = slowest_publish_us.load(Ordering::Relaxed);
    eprintln!(
        "gc soak: {}s, {WRITERS} writers, {checkins} checkins, {forks} forks, \
         {abandons} abandoned forks, {sweeps} sweeps, {unlinked} objects unlinked, \
         slowest put-to-publish {:.1}ms (floor {}s)",
        soak_secs(),
        slowest_us as f64 / 1000.0,
        GC_COLLECT_MIN_AGE_FLOOR
    );

    assert!(
        checkins + forks > 100,
        "the load was too thin to say anything: {checkins} checkins, {forks} forks"
    );
    assert!(
        unlinked > 0,
        "nothing was collected in {sweeps} sweeps, so this run proves nothing about \
         collection racing publication"
    );
    assert!(
        slowest_us < GC_COLLECT_MIN_AGE_FLOOR * 1_000_000,
        "a single put-to-publish interval took {slowest_us}us, which exceeds the \
         {GC_COLLECT_MIN_AGE_FLOOR}s floor: this run violated the precondition \
         collection is sound under, so its result is not evidence either way"
    );

    // The verdict. `fsck --full` rereads every durable byte, walks every
    // metadata root and every object file, and fails on any edge that does not
    // resolve -- which is exactly what a wrongly swept object leaves behind.
    let report = forge.fsck(&root, true).unwrap();
    assert!(
        report.findings.is_empty(),
        "collection racing publication left dangling references: {:?}",
        report.findings
    );

    // ...and again from cold, because the caches of the process that did the
    // collecting are not evidence about what is on disk.
    drop(forge);
    let cold = Forge::open(dir.path()).unwrap();
    let cold_root = cold.root_cap().unwrap();
    let cold_report = cold.fsck(&cold_root, true).unwrap();
    assert!(
        cold_report.findings.is_empty(),
        "a cold reopen found what the collecting process's caches hid: {:?}",
        cold_report.findings
    );
}
