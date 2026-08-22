from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one anchor, found {n}")
    return text.replace(old, new, 1)


p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''mod fsck;
mod serve;''',
    '''mod fsck;
mod serve;
mod soak;''',
    "soak module",
)
s = replace_once(
    s,
    '''pub use fsck::{FsckFinding, FsckReport};
pub use serve::serve;''',
    '''pub use fsck::{FsckFinding, FsckReport};
pub use serve::serve;
pub use soak::{
    private_checkins_bounded, run_bench_with_workers, shared_stampede_bounded,
};''',
    "soak exports",
)
p.write_text(s)


p = Path("crates/forge-api/src/bench.rs")
s = p.read_text()
s = replace_once(
    s,
    '''             private = N threads, private refs (throughput; p50 includes convoy wait).\n\\
             shared  = N threads, one ref; I8 pin ⇒ 1 Updated + N-1 Forked.\n",''',
    '''             private = N logical agents (CLI uses bounded workers), private refs.\n\\
             shared  = N pre-pinned writers; I8 pin ⇒ 1 Updated + N-1 Forked.\n",''',
    "benchmark wording",
)
p.write_text(s)


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
