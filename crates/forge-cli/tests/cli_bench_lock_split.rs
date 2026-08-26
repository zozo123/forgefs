//! #324: `forge bench` must attribute lock wait, not only total it.
//!
//! `MetaStats.lock_acquires` and `lock_wait_us` have summed the write
//! connection's mutex and the read pool since #315, so a writer convoy and a
//! busy read pool rendered as the same line. Every performance conclusion in
//! this project has come from these counters, and `lock_wait_us` alone has
//! already produced three wrong ones on #37, so the counter that cannot
//! attribute must not be the only one on the page.
//!
//! Renderer-level assertions live in `bench.rs`'s own tests, where two
//! synthetic `MetaStats` with identical sums are shown to render differently.
//! What this file adds is that the real binary, running real workloads,
//! actually emits the split and the read-heavy phase -- and that the split is
//! arithmetically the sum it decomposes.
//!
//! This is also where CI exercises W8. The smoke bench in `ci.yml` keeps its
//! exact argv: `.github/scripts/check-workflow-security.py` allowlists workflow
//! run commands verbatim, and widening that allowlist to add `--readers` would
//! be a change to the workflow-security policy inside a bench-instrumentation
//! change. `cargo test --workspace --all-targets --locked` runs this file, so
//! the phase and the split are covered on every push either way.

use std::process::Command;

fn bench(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("bench")
        .args(args)
        .output()
        .expect("spawn forge bench");
    assert!(
        out.status.success(),
        "forge bench {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn line<'a>(rendered: &'a str, prefix: &str) -> &'a str {
    rendered
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in:\n{rendered}"))
}

fn field(line: &str, key: &str) -> u64 {
    let token = line
        .split_whitespace()
        .find(|t| t.starts_with(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"));
    token[key.len() + 1..]
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in {line:?}"))
}

/// The split is emitted, and it is really the decomposition of the sum printed
/// above it: a reader must be able to check the arithmetic by eye rather than
/// trust two independently derived numbers.
#[test]
fn bench_renders_the_lock_split_and_it_adds_up_to_the_sum() {
    let rendered = bench(&[
        "--agents",
        "4",
        "--shared",
        "3",
        "--readers",
        "3",
        "--reads",
        "32",
        "--workers",
        "4",
    ]);

    let sum = line(&rendered, "sqlite lifetime");
    let split = line(&rendered, "sqlite locks");

    assert_eq!(
        field(sum, "lock_acquires"),
        field(split, "write_acquires") + field(split, "read_acquires"),
        "\n{sum}\n{split}"
    );
    assert_eq!(
        field(sum, "lock_wait_us"),
        field(split, "write_wait_us") + field(split, "read_wait_us"),
        "\n{sum}\n{split}"
    );
    assert!(
        split.contains("write_share_of_wait="),
        "the one number that answers \"was this a writer convoy?\": {split}"
    );
}

/// The read-heavy phase runs, produces one sample per read, and is the phase
/// that puts catalog traffic on the read pool. Without it every workload in
/// this harness is a checkin and the split has nothing to distinguish.
#[test]
fn the_read_heavy_phase_puts_traffic_on_the_read_pool() {
    let readers = 4;
    let reads = 48;
    let rendered = bench(&[
        "--agents",
        "2",
        "--shared",
        "0",
        "--readers",
        &readers.to_string(),
        "--reads",
        &reads.to_string(),
        "--workers",
        "4",
    ]);

    let phase = line(&rendered, "read fanout");
    assert!(
        phase.contains(&format!("readers={readers}"))
            && phase.contains(&format!("n={}", readers * reads)),
        "{phase}"
    );

    let split = line(&rendered, "sqlite locks");
    assert!(
        field(split, "read_acquires") > field(split, "write_acquires"),
        "a phase of {} reads over 8 paths must be read-pool dominated: {split}",
        readers * reads
    );
}

/// `--readers` defaults to 0, so an invocation written before the phase existed
/// runs exactly what it always ran -- and the split is still printed, because
/// the split is about the counters and not about the new workload.
#[test]
fn the_read_phase_is_off_by_default_and_the_split_is_not() {
    let rendered = bench(&["--agents", "2", "--shared", "2", "--workers", "2"]);
    assert!(
        !rendered.contains("read fanout"),
        "the read phase must not run unless asked for:\n{rendered}"
    );
    line(&rendered, "sqlite locks");
}
