use crate::bench::{merge_all_and_seal, serial_checkins, BenchReport, Percentiles};
use crate::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error, Result};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

fn us(d: Duration) -> u128 {
    d.as_micros()
}

/// Drive N logical private agents with at most `workers` OS threads.
///
/// Keeping logical agents independent from executor threads lets the harness
/// stress repository semantics at 1K+ agents without turning the benchmark
/// itself into an unbounded thread-creation test.
pub fn private_checkins_bounded(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
    workers: usize,
) -> Result<(Percentiles, Duration, usize)> {
    if n == 0 {
        return Ok((Percentiles::from_us(Vec::new()), Duration::ZERO, 0));
    }
    let workers = workers.max(1).min(n);
    let next = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(workers);

    for _ in 0..workers {
        let forge = forge.clone();
        let root = root.clone();
        let next = next.clone();
        handles.push(thread::spawn(move || -> Result<Vec<(u128, CasResult)>> {
            let mut out = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let t0 = Instant::now();
                let agent = forge.grant(
                    &root,
                    vec![
                        "ops=read,write,branch".into(),
                        format!("agent=bench{i}"),
                        "ref=heads/agents/*,main".into(),
                    ],
                )?;
                let ns = forge.session_open(&agent, "main")?;
                forge.write(
                    &agent,
                    &ns,
                    &format!("/w{i}.txt"),
                    format!("agent {i}").as_bytes(),
                    false,
                )?;
                let result = forge.checkin(&agent, &ns, "/", "bench")?;
                out.push((us(t0.elapsed()), result));
            }
            Ok(out)
        }));
    }

    let mut samples = Vec::with_capacity(n);
    let mut updated = 0usize;
    for handle in handles {
        for (latency, result) in handle
            .join()
            .map_err(|_| Error::Internal("bounded worker panic".into()))??
        {
            samples.push(latency);
            if matches!(result, CasResult::Updated { .. }) {
                updated += 1;
            }
        }
    }
    if samples.len() != n {
        return Err(Error::Internal(format!(
            "bounded private workload produced {} results for {n} agents",
            samples.len()
        )));
    }
    Ok((Percentiles::from_us(samples), start.elapsed(), updated))
}

/// Pre-pin N writers to one shared ref, then drive their checkins through a
/// bounded worker pool. Exactly one update and N-1 forks must survive batching.
pub fn shared_stampede_bounded(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
    workers: usize,
) -> Result<(Percentiles, Duration, usize, usize)> {
    if n == 0 {
        return Ok((Percentiles::from_us(Vec::new()), Duration::ZERO, 0, 0));
    }
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

    let prepared = Arc::new(prepared);
    let next = Arc::new(AtomicUsize::new(0));
    let workers = workers.max(1).min(n);
    let start = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let forge = forge.clone();
        let prepared = prepared.clone();
        let next = next.clone();
        handles.push(thread::spawn(move || -> Result<Vec<(u128, CasResult)>> {
            let mut out = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= prepared.len() {
                    break;
                }
                let (agent, ns) = prepared[i].clone();
                let t0 = Instant::now();
                let result = forge.checkin(&agent, &ns, "/", "shared")?;
                out.push((us(t0.elapsed()), result));
            }
            Ok(out)
        }));
    }

    let mut samples = Vec::with_capacity(n);
    let mut updated = 0usize;
    let mut forked = 0usize;
    for handle in handles {
        for (latency, result) in handle
            .join()
            .map_err(|_| Error::Internal("bounded worker panic".into()))??
        {
            samples.push(latency);
            match result {
                CasResult::Updated { .. } => updated += 1,
                CasResult::Forked { .. } => forked += 1,
                CasResult::Noop { .. } => {}
            }
        }
    }
    if samples.len() != n {
        return Err(Error::Internal(format!(
            "bounded shared workload produced {} results for {n} agents",
            samples.len()
        )));
    }
    Ok((
        Percentiles::from_us(samples),
        start.elapsed(),
        updated,
        forked,
    ))
}

/// CLI benchmark runner with logical-agent count separated from OS worker count.
pub fn run_bench_with_workers(
    dir: &std::path::Path,
    agents: usize,
    shared: usize,
    workers: usize,
) -> Result<BenchReport> {
    let forge = Arc::new(Forge::init(dir)?);
    let root = forge.root_cap()?;
    let integ = forge.integrator_cap()?;
    let serial = serial_checkins(&forge, &root, 8.min(agents))?;
    let private = private_checkins_bounded(forge.clone(), &root, agents, workers)?;
    let shared_result = shared_stampede_bounded(forge.clone(), &root, shared, workers)?;
    let merge_seal = merge_all_and_seal(&forge, &root, &integ, "bench")?;
    let t0 = Instant::now();
    forge.verify_tag(&root, "bench")?;
    let verify = t0.elapsed();
    let fsck = forge.fsck(&root, true)?;
    if !fsck.ok {
        return Err(Error::Corrupt(format!(
            "benchmark repository failed full fsck with {} finding(s)",
            fsck.findings.len()
        )));
    }
    let store = forge.store.stats();
    Ok(BenchReport {
        serial: Some(serial),
        private: Some(private),
        shared: Some(shared_result),
        merge_seal: Some(merge_seal),
        verify: Some(verify),
        durability: Some(forge.store.meta.durability_policy().clone()),
        store: Some(store),
        meta: Some(forge.store.meta.stats()),
        api: Some(forge.api_stats()),
    })
}
