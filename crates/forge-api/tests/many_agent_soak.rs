use forge_api::{private_checkins_bounded, shared_stampede_bounded, Forge};
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn private_256_bounded_then_full_fsck() {
    let d = tempdir().unwrap();
    let forge = Arc::new(Forge::init(d.path()).unwrap());
    let root = forge.root_cap().unwrap();

    let (p, wall, updated) = private_checkins_bounded(forge.clone(), &root, 256, 32).unwrap();
    assert_eq!(updated, 256);
    assert_eq!(p.n, 256);
    eprintln!(
        "private256 workers=32 wall={:.3}s hz={:.1} p50={:.2}ms p95={:.2}ms p99={:.2}ms",
        wall.as_secs_f64(),
        p.throughput_hz(wall),
        p.p50_us as f64 / 1000.0,
        p.p95_us as f64 / 1000.0,
        p.p99_us as f64 / 1000.0
    );

    let report = forge.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
}

#[test]
#[ignore = "explicit large soak; run with --ignored"]
fn private_1024_and_shared_256_survive_reopen_and_fsck() {
    let d = tempdir().unwrap();
    let forge = Arc::new(Forge::init(d.path()).unwrap());
    let root = forge.root_cap().unwrap();

    let (p, wall, updated) = private_checkins_bounded(forge.clone(), &root, 1024, 64).unwrap();
    assert_eq!(updated, 1024);
    assert_eq!(p.n, 1024);
    eprintln!(
        "private1024 workers=64 wall={:.3}s hz={:.1} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        wall.as_secs_f64(),
        p.throughput_hz(wall),
        p.p50_us as f64 / 1000.0,
        p.p95_us as f64 / 1000.0,
        p.p99_us as f64 / 1000.0,
        p.max_us as f64 / 1000.0
    );

    let (p, wall, winners, forks) = shared_stampede_bounded(forge.clone(), &root, 256, 64).unwrap();
    assert_eq!(winners, 1);
    assert_eq!(forks, 255);
    assert_eq!(p.n, 256);
    eprintln!(
        "shared256 workers=64 wall={:.3}s winner={winners} forks={forks} p50={:.2}ms p99={:.2}ms",
        wall.as_secs_f64(),
        p.p50_us as f64 / 1000.0,
        p.p99_us as f64 / 1000.0
    );

    forge.seal(&root, "main", "soak-1024").unwrap();
    forge.verify_tag(&root, "soak-1024").unwrap();
    let before = forge.fsck(&root, true).unwrap();
    assert!(before.ok, "{:#?}", before.findings);

    drop(root);
    drop(forge);

    let reopened = Forge::open(d.path()).unwrap();
    let root = reopened.root_cap().unwrap();
    reopened.verify_tag(&root, "soak-1024").unwrap();
    let after = reopened.fsck(&root, true).unwrap();
    assert!(after.ok, "{:#?}", after.findings);
}
