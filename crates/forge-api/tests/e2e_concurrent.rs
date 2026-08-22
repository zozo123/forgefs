//! In-process e2e: private throughput, shared-ref stampede, seal, stale reads.

use forge_api::{merge_all_and_seal, private_checkins, shared_stampede, Forge};
use forge_types::{CasResult, Error};
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn e2e_private_32_all_updated() {
    let d = tempdir().unwrap();
    let f = Arc::new(Forge::init(d.path()).unwrap());
    let root = f.root_cap().unwrap();
    let (p, wall, ok) = private_checkins(f, &root, 32).unwrap();
    assert_eq!(ok, 32, "lost a private checkin");
    assert_eq!(p.n, 32);
    eprintln!(
        "private32 wall={:.3}s p50={:.2}ms p95={:.2}ms hz={:.1}",
        wall.as_secs_f64(),
        p.p50_us as f64 / 1000.0,
        p.p95_us as f64 / 1000.0,
        p.throughput_hz(wall)
    );
}

#[test]
fn e2e_shared_16_one_winner_rest_fork() {
    let d = tempdir().unwrap();
    let f = Arc::new(Forge::init(d.path()).unwrap());
    let root = f.root_cap().unwrap();
    let (p, wall, updated, forked) = shared_stampede(f, &root, 16).unwrap();
    assert_eq!(updated + forked, 16, "{updated}+{forked}");
    assert_eq!(
        updated, 1,
        "shared stampede must have exactly one CAS winner"
    );
    assert_eq!(forked, 15);
    eprintln!(
        "shared16 wall={:.3}s updated={updated} forked={forked} p50={:.2}ms",
        wall.as_secs_f64(),
        p.p50_us as f64 / 1000.0
    );
}

#[test]
fn e2e_merge_seal_verify_after_private() {
    let d = tempdir().unwrap();
    let f = Arc::new(Forge::init(d.path()).unwrap());
    let root = f.root_cap().unwrap();
    let integ = f.integrator_cap().unwrap();
    let (_, _, ok) = private_checkins(f.clone(), &root, 8).unwrap();
    assert_eq!(ok, 8);
    let merge_t = merge_all_and_seal(&f, &root, &integ, "e2e").unwrap();
    f.verify_tag(&root, "e2e").unwrap();
    eprintln!("merge+seal 8 agents {:.3}s", merge_t.as_secs_f64());
}

#[test]
fn e2e_two_agents_disjoint_then_seal() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let cap = f.root_cap().unwrap();
    let integ = f.integrator_cap().unwrap();
    let a = f.session_open(&cap, "main").unwrap();
    let b = f.session_open(&cap, "main").unwrap();
    f.write(&cap, &a, "/paper.txt", b"hello", false).unwrap();
    f.write(&cap, &b, "/code.rs", b"fn main(){}", false)
        .unwrap();
    let CasResult::Updated { name: ra, .. } = f.checkin(&cap, &a, "/", "paper").unwrap() else {
        panic!("a");
    };
    let CasResult::Updated { name: rb, .. } = f.checkin(&cap, &b, "/", "code").unwrap() else {
        panic!("b");
    };
    f.merge(&integ, "main", &ra, None).unwrap();
    f.merge(&integ, "main", &rb, None).unwrap();
    f.seal(&integ, "main", "v1.0").unwrap();
    f.verify_tag(&cap, "v1.0").unwrap();
}

#[test]
fn e2e_stale_read_is_not_a_silent_success() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let cap = f.root_cap().unwrap();
    let integ = f.integrator_cap().unwrap();
    let alice = f
        .grant(
            &cap,
            vec![
                "ops=read,write,branch".into(),
                "agent=alice".into(),
                "ref=heads/agents/alice/*,main".into(),
            ],
        )
        .unwrap();
    let bob = f
        .grant(
            &cap,
            vec![
                "ops=read,write,branch".into(),
                "agent=bob".into(),
                "ref=heads/agents/bob/*,main".into(),
            ],
        )
        .unwrap();
    let a = f.session_open(&alice, "main").unwrap();
    f.write(&alice, &a, "/doc.txt", b"v1", false).unwrap();
    let CasResult::Updated { name, .. } = f.checkin(&alice, &a, "/", "v1").unwrap() else {
        panic!();
    };
    f.merge(&integ, "main", &name, None).unwrap();
    let b = f.session_open(&bob, "main").unwrap();
    assert_eq!(f.read(&bob, &b, "/main/doc.txt").unwrap(), b"v1");
    let a2 = f.session_open(&alice, "main").unwrap();
    f.write(&alice, &a2, "/doc.txt", b"v2", false).unwrap();
    let CasResult::Updated { name: n2, .. } = f.checkin(&alice, &a2, "/", "v2").unwrap() else {
        panic!();
    };
    f.merge(&integ, "main", &n2, None).unwrap();
    f.write(&bob, &b, "/notes.txt", b"hi", false).unwrap();

    // This API outcome stays exact even when SQLite and storage counters are enabled too.
    let before = f.api_stats();
    let err = f.checkin(&bob, &b, "/", "notes").unwrap_err();
    assert!(matches!(err, Error::StaleObservation { .. }), "{err:?}");
    let after = f.api_stats();
    assert_eq!(after.stale_observation, before.stale_observation + 1);
}
