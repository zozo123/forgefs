//! The concurrent proof for I19/I21. This is the code path that wedged before:
//! a read through a second read-write mount recorded an observation the checkin
//! side could never agree with, so the session failed every checkin forever, and
//! a checkin of such a mount CASed one ref from another ref's commit.
//!
//! Many sessions, each holding read-write mounts on several distinct refs, read
//! and write through all of them under contention and then try to publish every
//! one. Two properties are asserted:
//!
//!   * LIVENESS (I21): every session reaches a terminal state for every mount it
//!     wrote through -- published, forked, or explicitly abandoned. A session
//!     that ends holding staged work it cannot publish is a wedge.
//!   * ISOLATION (I19): no ref ends up holding another ref's content. Each ref
//!     is seeded with a marker only it has, and every path written through a
//!     mount of ref R is named after R, so a cross-ref leak is visible by name.
//!
//! Then `fsck --full` must be clean, which is also what proves the new mount
//! pins are rooted: a pin the walk does not know about would be unreachable.

use forge_api::Forge;
use forge_types::{CasResult, Error};
use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

const REFS: &[&str] = &["alpha", "beta", "gamma", "delta"];
const AGENTS: usize = 16;
const ROUNDS: usize = 6;

/// The default shape is what CI runs on every commit. A longer soak is the same
/// harness with more of it, so the scale is an env override rather than a second
/// test that could drift from this one:
/// `FORGEFS_MULTI_MOUNT_AGENTS=48 FORGEFS_MULTI_MOUNT_ROUNDS=24 cargo test ...`
fn scale(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[derive(Default, Debug)]
struct Tally {
    updated: usize,
    forked: usize,
    noop: usize,
    stale: usize,
    busy: usize,
    other: usize,
}

impl Tally {
    fn merge(&mut self, o: &Tally) {
        self.updated += o.updated;
        self.forked += o.forked;
        self.noop += o.noop;
        self.stale += o.stale;
        self.busy += o.busy;
        self.other += o.other;
    }
}

#[test]
fn many_sessions_with_many_read_write_mounts_never_wedge_and_never_cross_refs() {
    let d = tempdir().unwrap();
    let f = Arc::new(Forge::init(d.path()).unwrap());
    let root = f.root_cap().unwrap();

    // Each ref gets a marker only it holds, so any leak is visible by name.
    for r in REFS {
        f.branch(&root, "main", r).unwrap();
        let ns = f.session_open(&root, r).unwrap();
        f.mount(&root, &ns, "/", &format!("ref:{r}"), true).unwrap();
        f.write(&root, &ns, &format!("/marker-{r}.txt"), r.as_bytes(), false)
            .unwrap();
        assert!(matches!(
            f.checkin(&root, &ns, "/", "seed").unwrap(),
            CasResult::Updated { .. }
        ));
    }

    let agents = scale("FORGEFS_MULTI_MOUNT_AGENTS", AGENTS);
    let rounds = scale("FORGEFS_MULTI_MOUNT_ROUNDS", ROUNDS);
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for agent in 0..agents {
        let f = Arc::clone(&f);
        let root = root.clone();
        handles.push(thread::spawn(move || {
            let mut tally = Tally::default();
            let mut wedged: Vec<String> = Vec::new();
            let mut ops = 0usize;
            for round in 0..rounds {
                // Rotate which ref is the session base so every ref is somebody's
                // own base and somebody else's foreign read-write mount.
                let own = REFS[(agent + round) % REFS.len()];
                let ns = f.session_open(&root, own).unwrap();
                ops += 1;
                f.mount(&root, &ns, "/", &format!("ref:{own}"), true)
                    .unwrap();
                let mut mounts = vec![("/".to_string(), own)];
                let mut live = Vec::new();
                for (i, r) in REFS.iter().enumerate() {
                    if r == &own {
                        continue;
                    }
                    let path = format!("/m{i}");
                    // Alternate the mode so both the pinned and the live path
                    // are exercised under contention.
                    let rw = (agent + i + round) % 3 != 0;
                    f.mount(&root, &ns, &path, &format!("ref:{r}"), rw).unwrap();
                    ops += 1;
                    if rw {
                        mounts.push((path, r));
                    } else {
                        live.push((path, *r));
                    }
                }

                // A read-only mount resolves live on purpose, so reading one
                // under contention is what SHOULD go stale (I9). Doing it here
                // keeps the liveness claim honest: the run covers both the
                // pinned and the live path, and a stale read must be clearable
                // by re-reading rather than being a wedge.
                for (path, r) in &live {
                    let _ = f.ls(&root, &ns, path);
                    let _ = f.read(&root, &ns, &format!("{path}/marker-{r}.txt"));
                    ops += 2;
                }

                // Read through every writable mount -- this is what used to
                // wedge the session -- then write a path named after the ref the
                // mount points at.
                for (path, r) in &mounts {
                    let prefix = if path == "/" { "" } else { path.as_str() };
                    let _ = f.ls(&root, &ns, path);
                    let _ = f.read(&root, &ns, &format!("{prefix}/marker-{r}.txt"));
                    let _ = f.read(&root, &ns, &format!("{prefix}/absent-{agent}.txt"));
                    ops += 3;
                    f.write(
                        &root,
                        &ns,
                        &format!("{prefix}/from-{r}-{agent}-{round}.txt"),
                        r.as_bytes(),
                        false,
                    )
                    .unwrap();
                    ops += 1;
                }

                // Every mount that accepted a write must reach a terminal state.
                for (path, _) in &mounts {
                    let mut settled = false;
                    // A StaleObservation from a live read-only mount is
                    // clearable by re-reading; give it a bounded number of
                    // attempts, which is exactly what a caller can do.
                    for _ in 0..16 {
                        ops += 1;
                        match f.checkin(&root, &ns, path, "work") {
                            Ok(CasResult::Updated { .. }) => {
                                tally.updated += 1;
                                settled = true;
                                break;
                            }
                            Ok(CasResult::Forked { .. }) => {
                                tally.forked += 1;
                                settled = true;
                                break;
                            }
                            Ok(CasResult::Noop { .. }) => {
                                tally.noop += 1;
                                settled = true;
                                break;
                            }
                            Err(Error::StaleObservation { .. }) => {
                                tally.stale += 1;
                                // Re-read what went stale, then retry. The
                                // escape has to be derivable from the error, so
                                // it is exactly "read it again".
                                for (p, r) in &live {
                                    let _ = f.ls(&root, &ns, p);
                                    let _ = f.read(&root, &ns, &format!("{p}/marker-{r}.txt"));
                                    ops += 2;
                                }
                            }
                            Err(Error::Busy(_)) => tally.busy += 1,
                            Err(other) => {
                                tally.other += 1;
                                wedged.push(format!("{ns}:{path}: {other:?}"));
                                break;
                            }
                        }
                    }
                    if !settled {
                        wedged.push(format!("{ns}:{path}: never reached a terminal state"));
                    }
                }

                // The session must be retirable without discarding work: that is
                // the third terminal state, and refusing it means work is stuck.
                if let Err(error) = f.abandon_session(&root, &ns, false) {
                    wedged.push(format!("{ns}: abandon refused: {error:?}"));
                }
                ops += 1;
            }
            (tally, wedged, ops)
        }));
    }

    let mut total = Tally::default();
    let mut wedged = Vec::new();
    let mut ops = 0usize;
    for h in handles {
        let (t, w, o) = h.join().expect("agent thread panicked");
        total.merge(&t);
        wedged.extend(w);
        ops += o;
    }
    let wall = started.elapsed();

    assert!(
        wedged.is_empty(),
        "I21: {} session/mount pairs never reached a terminal state:\n{}",
        wedged.len(),
        wedged.join("\n")
    );
    assert!(
        total.updated + total.forked > 0,
        "the run published nothing: {total:?}"
    );

    // ISOLATION: every ref holds only its own marker and only paths named after
    // itself. A ref holding another ref's content is the disaster this fix is
    // about.
    for r in REFS {
        let ns = f.session_open(&root, r).unwrap();
        f.mount(&root, &ns, "/", &format!("ref:{r}"), false)
            .unwrap();
        for entry in f.ls(&root, &ns, "/").unwrap() {
            let name = entry.0;
            if let Some(rest) = name.strip_prefix("marker-") {
                assert_eq!(
                    rest.trim_end_matches(".txt"),
                    *r,
                    "ref {r} holds another ref's marker: {name}"
                );
                continue;
            }
            if let Some(rest) = name.strip_prefix("from-") {
                let owner = rest.split('-').next().unwrap();
                assert_eq!(
                    owner, *r,
                    "ref {r} holds content written for {owner}: {name}"
                );
                continue;
            }
            panic!("ref {r} holds an unexpected entry {name}");
        }
    }

    // Forks are the I18/I5 loser path and are refs like any other, so they are
    // held to the same rule.
    for row in f.refs(&root).unwrap() {
        let Some(rest) = row.name.strip_prefix("forks/") else {
            continue;
        };
        let owner = rest.split('/').next().unwrap().to_string();
        let ns = f.session_open(&root, &row.name).unwrap();
        f.mount(&root, &ns, "/", &format!("ref:{}", row.name), false)
            .unwrap();
        for entry in f.ls(&root, &ns, "/").unwrap() {
            let name = entry.0;
            let expected = name
                .strip_prefix("marker-")
                .map(|rest| rest.trim_end_matches(".txt").to_string())
                .or_else(|| {
                    name.strip_prefix("from-")
                        .map(|rest| rest.split('-').next().unwrap().to_string())
                });
            assert_eq!(
                expected.as_deref(),
                Some(owner.as_str()),
                "fork {} of {owner} holds {name}",
                row.name
            );
        }
    }

    let report = f.fsck(&root, true).unwrap();
    assert!(
        report.ok && report.findings.is_empty(),
        "fsck --full after the concurrent run: {:?}",
        report.findings
    );
    eprintln!(
        "multi-mount concurrent: agents={agents} rounds={rounds} refs={} ops={ops} \
         wall={:.3}s updated={} forked={} noop={} stale-retried={} busy={} \
         fsck_objects={}",
        REFS.len(),
        wall.as_secs_f64(),
        total.updated,
        total.forked,
        total.noop,
        total.stale,
        total.busy,
        report.checked_objects
    );
}
