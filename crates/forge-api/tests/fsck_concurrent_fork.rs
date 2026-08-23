//! fsck must not report corruption on a healthy repository just because a
//! concurrent checkin forked while it was scanning.
//!
//! fsck reads refs, then namespaces, then each namespace's mounts as separate
//! queries. A checkin that loses a CAS atomically inserts a
//! `forks/<ref>/<agent>/<ulid>` and repoints the losing session's mount at it,
//! so an fsck that snapshotted refs before that commit and read mounts after it
//! saw a mount naming a ref it never loaded, and reported MOUNT_REF -- exit 2,
//! the code CLI_ABI.md reserves for corruption, on intact bytes.
//!
//! Forking is the designed outcome of losing a race, so the trigger is ordinary
//! contention. Any CI or release gate keyed on `fsck --full` would fail
//! intermittently and claim corruption.

use forge_api::Forge;
use forge_types::{CasResult, Error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn fsck_never_claims_corruption_while_concurrent_checkins_fork() {
    let d = tempdir().unwrap();
    let f = Arc::new(Forge::init(d.path()).unwrap());
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();

    let seed = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &seed, "/", "ref:shared", true).unwrap();
    f.write(&root, &seed, "/seed.txt", b"v0", false).unwrap();
    f.checkin(&root, &seed, "/", "seed").unwrap();

    let stop = Arc::new(AtomicBool::new(false));

    // Two sessions per round both open from `shared` and both check in, so the
    // second reliably loses the CAS and forks. Sessions also accumulate, which
    // lengthens fsck's per-namespace loop and widens the window on purpose.
    let wf = f.clone();
    let wroot = root.clone();
    let wstop = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut forks = 0usize;
        for i in 0..60 {
            if wstop.load(Ordering::Relaxed) {
                break;
            }
            let a = wf.session_open(&wroot, "shared").unwrap();
            wf.mount(&wroot, &a, "/", "ref:shared", true).unwrap();
            let b = wf.session_open(&wroot, "shared").unwrap();
            wf.mount(&wroot, &b, "/", "ref:shared", true).unwrap();
            wf.write(&wroot, &a, &format!("/a{i}.txt"), b"a", false)
                .unwrap();
            wf.write(&wroot, &b, &format!("/b{i}.txt"), b"b", false)
                .unwrap();
            let _ = wf.checkin(&wroot, &a, "/", "a");
            if let Ok(CasResult::Forked { .. }) = wf.checkin(&wroot, &b, "/", "b") {
                forks += 1;
            }
        }
        forks
    });

    let mut scans = 0usize;
    while !writer.is_finished() {
        match f.fsck(&root, false) {
            Ok(report) => assert!(
                report.ok,
                "fsck reported corruption on a healthy repository: {:?}",
                report.findings
            ),
            // Contention is legitimate and has its own exit class; corruption is not.
            Err(Error::Busy(_)) => {}
            Err(Error::Corrupt(detail)) => panic!("fsck claimed corruption: {detail}"),
            Err(other) => panic!("fsck failed unexpectedly: {other}"),
        }
        scans += 1;
        if scans > 20_000 {
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    let forks = writer.join().unwrap();
    assert!(
        forks > 0,
        "no checkin forked, so this run never exercised the race"
    );
    assert!(scans > 0, "fsck never ran concurrently with the writer");

    let final_report = f.fsck(&root, true).unwrap();
    assert!(
        final_report.ok,
        "final fsck --full dirty: {:?}",
        final_report.findings
    );
}
