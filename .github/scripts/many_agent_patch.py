from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one anchor, found {n}")
    return text.replace(old, new, 1)


def replace_block(text: str, start: str, end: str, new: str, label: str) -> str:
    a = text.find(start)
    if a < 0:
        raise SystemExit(f"{label}: start missing")
    b = text.find(end, a)
    if b < 0:
        raise SystemExit(f"{label}: end missing")
    return text[:a] + new + text[b:]


p = Path("crates/forge-api/src/bench.rs")
s = p.read_text()
s = replace_once(
    s,
    '''use std::sync::Arc;
use std::thread;''',
    '''use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;''',
    "bench imports",
)
s = replace_once(
    s,
    '''             private = N threads, private refs (throughput; p50 includes convoy wait).\n\\
             shared  = N threads, one ref; I8 pin ⇒ 1 Updated + N-1 Forked.\n",''',
    '''             private = N logical agents over bounded workers, private refs.\n\\
             shared  = N pre-pinned writers over bounded workers; 1 Updated + N-1 Forked.\n",''',
    "bench description",
)

private_impl = '''/// N logical agents over a bounded worker pool, each with a private live ref.
/// All checkins must be `Updated`; worker count controls process pressure independently
/// from the logical agent count.
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
        let f = forge.clone();
        let cap = root.clone();
        let next = next.clone();
        handles.push(thread::spawn(move || -> Result<Vec<(u128, CasResult)>> {
            let mut out = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
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
                let result = f.checkin(&agent, &ns, "/", "bench")?;
                out.push((us(t0.elapsed()), result));
            }
            Ok(out)
        }));
    }

    let mut samples = Vec::with_capacity(n);
    let mut updated = 0usize;
    for handle in handles {
        for (dt, result) in handle
            .join()
            .map_err(|_| Error::Internal("worker panic".into()))??
        {
            samples.push(dt);
            if matches!(result, CasResult::Updated { .. }) {
                updated += 1;
            }
        }
    }
    if samples.len() != n {
        return Err(Error::Internal(format!(
            "bounded private bench produced {} results for {n} agents",
            samples.len()
        )));
    }
    Ok((Percentiles::from_us(samples), start.elapsed(), updated))
}

/// Compatibility helper: logical concurrency is bounded at 64 workers.
pub fn private_checkins(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
) -> Result<(Percentiles, Duration, usize)> {
    private_checkins_bounded(forge, root, n, n.clamp(1, 64))
}

'''
s = replace_block(
    s,
    "/// N agents, private live refs, unique files. All checkins should `Updated`.",
    "/// N agents remount the same shared ref.",
    private_impl,
    "private bounded block",
)

shared_impl = '''/// N pre-pinned writers race one shared ref through a bounded worker pool.
/// Snapshot pinning still requires exactly one Updated and N-1 Forked outcomes.
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
        let f = forge.clone();
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
                let result = f.checkin(&agent, &ns, "/", "shared")?;
                out.push((us(t0.elapsed()), result));
            }
            Ok(out)
        }));
    }

    let mut samples = Vec::with_capacity(n);
    let mut updated = 0usize;
    let mut forked = 0usize;
    for handle in handles {
        for (dt, result) in handle
            .join()
            .map_err(|_| Error::Internal("worker panic".into()))??
        {
            samples.push(dt);
            match result {
                CasResult::Updated { .. } => updated += 1,
                CasResult::Forked { .. } => forked += 1,
                CasResult::Noop { .. } => {}
            }
        }
    }
    if samples.len() != n {
        return Err(Error::Internal(format!(
            "bounded shared bench produced {} results for {n} agents",
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

/// Compatibility helper: logical concurrency is bounded at 64 workers.
pub fn shared_stampede(
    forge: Arc<Forge>,
    root: &Cap,
    n: usize,
) -> Result<(Percentiles, Duration, usize, usize)> {
    shared_stampede_bounded(forge, root, n, n.clamp(1, 64))
}

'''
s = replace_block(
    s,
    "/// N agents remount the same shared ref.",
    "/// Integrator folds every `heads/agents/*` into main, then seals.",
    shared_impl,
    "shared bounded block",
)

run_impl = '''pub fn run_with_workers(
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
    let shared_r = shared_stampede_bounded(forge.clone(), &root, shared, workers)?;
    let merge_seal = merge_all_and_seal(&forge, &root, &integ, "bench")?;
    let t0 = Instant::now();
    forge.verify_tag(&root, "bench")?;
    let fsck = forge.fsck(&root, true)?;
    if !fsck.ok {
        return Err(Error::Corrupt(format!(
            "benchmark repository failed fsck with {} finding(s)",
            fsck.findings.len()
        )));
    }
    Ok(BenchReport {
        serial: Some(serial),
        private: Some(private),
        shared: Some(shared_r),
        merge_seal: Some(merge_seal),
        verify: Some(t0.elapsed()),
    })
}

pub fn run(dir: &std::path::Path, agents: usize, shared: usize) -> Result<BenchReport> {
    run_with_workers(dir, agents, shared, agents.max(shared).clamp(1, 64))
}
'''
s = replace_block(s, "pub fn run(dir:", "}", run_impl, "run block")
# replace_block above stops at the first closing brace in the function signature body;
# remove any old tail if the original constructor body remains after our insertion.
old_tail = '''    let forge = Arc::new(Forge::init(dir)?);
    let root = forge.root_cap()?;
    let integ = forge.integrator_cap()?;
    let serial = serial_checkins(&forge, &root, 8.min(agents))?;
    let private = private_checkins(forge.clone(), &root, agents)?;
    let shared_r = shared_stampede(forge.clone(), &root, shared)?;
    let merge_seal = merge_all_and_seal(&forge, &root, &integ, "bench")?;
    let t0 = Instant::now();
    forge.verify_tag(&root, "bench")?;
    Ok(BenchReport {
        serial: Some(serial),
        private: Some(private),
        shared: Some(shared_r),
        merge_seal: Some(merge_seal),
        verify: Some(t0.elapsed()),
    })
}
'''
if old_tail in s:
    s = s.replace(old_tail, "", 1)
p.write_text(s)


# Public API exports.
p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''pub use bench::{
    merge_all_and_seal, private_checkins, run as run_bench, shared_stampede, BenchReport,
};''',
    '''pub use bench::{
    merge_all_and_seal, private_checkins, private_checkins_bounded, run as run_bench,
    run_with_workers as run_bench_with_workers, shared_stampede, shared_stampede_bounded,
    BenchReport,
};''',
    "bench exports",
)
p.write_text(s)


# CLI worker control.
p = Path("crates/forge-cli/src/main.rs")
s = p.read_text()
s = replace_once(
    s,
    '''    Bench {
        #[arg(long, default_value_t = 32)]
        agents: usize,
        #[arg(long, default_value_t = 16)]
        shared: usize,
    },''',
    '''    Bench {
        #[arg(long, default_value_t = 32)]
        agents: usize,
        #[arg(long, default_value_t = 16)]
        shared: usize,
        /// Maximum OS workers driving logical agents.
        #[arg(long, default_value_t = 64)]
        workers: usize,
    },''',
    "bench CLI args",
)
s = replace_once(
    s,
    '''        Cmd::Bench { agents, shared } => {
            let dir = cli.dir.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("forge-bench-{}", std::process::id()))
            });
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            eprintln!("forge bench dir={}", dir.display());
            let report = forge_api::run_bench(&dir, agents, shared)?;''',
    '''        Cmd::Bench {
            agents,
            shared,
            workers,
        } => {
            let dir = cli.dir.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("forge-bench-{}", std::process::id()))
            });
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            eprintln!(
                "forge bench dir={} agents={agents} shared={shared} workers={workers}",
                dir.display()
            );
            let report = forge_api::run_bench_with_workers(&dir, agents, shared, workers)?;''',
    "bench CLI dispatch",
)
p.write_text(s)
