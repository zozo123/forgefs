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
use forge_store::{blob::BlobStoreStats, DurabilityPolicy, MetaStats};
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
    pub merge_seal: Option<Duration>,
    pub verify: Option<Duration>,
    pub durability: Option<DurabilityPolicy>,
    pub store: Option<BlobStoreStats>,
    pub meta: Option<MetaStats>,
    pub api: Option<ApiStats>,
}

impl BenchReport {
    pub fn render(&self) -> String {
        let mut s = String::from(
            "ForgeFS e2e bench (durable puts: fsync file+dir)\n\
             serial = one agent at a time (true op latency).\n\
             private = N threads, private refs (throughput; p50 includes convoy wait).\n\
             shared  = N threads, one ref; I8 pin ⇒ 1 Updated + N-1 Forked.\n",
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
        if let Some(stats) = self.store {
            s.push_str(&format!(
                "storage          puts={} fsync_file={} fsync_dir={}\n",
                stats.puts, stats.fsync_file, stats.fsync_dir
            ));
        }
        if let Some(stats) = self.meta {
            s.push_str(&format!(
                "sqlite           txn={:.3}ms busy={} updated={} forked={} denied={}\n",
                stats.txn_us as f64 / 1000.0,
                stats.busy,
                stats.cas_updated,
                stats.cas_forked,
                stats.cas_denied,
            ));
        }
        if let Some(stats) = self.api {
            let wall_s = self
                .private
                .as_ref()
                .map(|(_, d, _)| d.as_secs_f64())
                .unwrap_or(0.0)
                + self
                    .shared
                    .as_ref()
                    .map(|(_, d, _, _)| d.as_secs_f64())
                    .unwrap_or(0.0);
            let rate = |n: u64| if wall_s > 0.0 { n as f64 / wall_s } else { 0.0 };
            s.push_str(&format!(
                "api outcomes     stale={} ({:.2}/s) conflict={} ({:.2}/s)\n",
                stats.stale_observation,
                rate(stats.stale_observation),
                stats.merge_conflict,
                rate(stats.merge_conflict),
            ));
        }
        s
    }
}

fn us(d: Duration) -> u128 {
    d.as_micros()
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
        merge_seal: Some(merge_seal),
        verify: Some(verify),
        durability: Some(forge.store.meta.durability_policy().clone()),
        store: Some(store),
        meta: Some(forge.store.meta.stats()),
        api: Some(forge.api_stats()),
    })
}
