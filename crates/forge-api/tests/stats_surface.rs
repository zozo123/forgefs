//! Issue #42: the structured metrics surface must count the outcomes it
//! claims to count, not merely exist.
//!
//! Counter scope is process lifetime (AGENTS.md test rules), so this asserts
//! movement between two snapshots taken inside one process. It never asserts
//! an absolute value, a rate, or anything that could be read as performance
//! evidence.

use forge_api::{Forge, STATS_SCHEMA_VERSION, STATS_SCOPE};
use forge_types::CasResult;
use tempfile::tempdir;

#[test]
fn counters_move_for_sessions_dedup_noop_checkin_and_merge() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    // `main` is protected, so all mutation happens on a normal branch.
    f.branch(&root, "main", "shared").unwrap();

    let before = f.stats_report();
    assert_eq!(before.schema_version, STATS_SCHEMA_VERSION);
    assert_eq!(before.scope, STATS_SCOPE);

    // I8: a session pin is its own commit point and had no counter of its own.
    let one = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &one, "/", "ref:shared", true).unwrap();
    f.write(&root, &one, "/a.txt", b"same-bytes", false)
        .unwrap();
    assert!(matches!(
        f.checkin(&root, &one, "/", "one").unwrap(),
        CasResult::Updated { .. }
    ));

    // I3: identical bytes must be satisfied by the existing object, which is
    // deliberately not a `put` and performs no new file barrier.
    let two = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &two, "/", "ref:shared", true).unwrap();
    f.write(&root, &two, "/b.txt", b"same-bytes", false)
        .unwrap();
    assert!(matches!(
        f.checkin(&root, &two, "/", "two").unwrap(),
        CasResult::Updated { .. }
    ));

    // A checkin that reproduces its pinned tree publishes no commit and
    // attempts no ref CAS, so it is none of updated/forked/denied.
    let three = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &three, "/", "ref:shared", true).unwrap();
    assert!(matches!(
        f.checkin(&root, &three, "/", "noop").unwrap(),
        CasResult::Noop { .. }
    ));

    // I11/I12: a clean merge produces a merge commit and reaches the ref CAS.
    f.branch(&root, "shared", "topic").unwrap();
    let four = f.session_open(&root, "topic").unwrap();
    f.mount(&root, &four, "/", "ref:topic", true).unwrap();
    f.write(&root, &four, "/c.txt", b"topic", false).unwrap();
    f.checkin(&root, &four, "/", "topic").unwrap();
    f.merge(&root, "shared", "topic", None).unwrap();

    let after = f.stats_report();
    assert_eq!(
        after.api.sessions_opened - before.api.sessions_opened,
        4,
        "every session_open must be counted exactly once"
    );
    assert_eq!(
        after.api.merge_applied - before.api.merge_applied,
        1,
        "a merge that reached the ref CAS must be counted"
    );
    assert_eq!(
        after.sqlite.cas_noop - before.sqlite.cas_noop,
        1,
        "a no-op checkin must be counted, not silently dropped"
    );
    assert!(
        after.sqlite.cas_updated > before.sqlite.cas_updated,
        "ref CAS updates must move"
    );
    assert!(
        after.store.puts > before.store.puts,
        "new objects must move the put counter"
    );
    assert!(
        after.store.dedup_hits > before.store.dedup_hits,
        "republishing identical bytes must be counted as a dedup hit, not a put"
    );
    assert!(
        after.sqlite.accounted_us >= after.sqlite.txn_us,
        "accounted_us is the saturating sum that includes txn_us"
    );
    assert_eq!(
        after.store.barrier_us,
        after.store.fsync_file_us + after.store.fsync_dir_us + after.store.barrier_fs_us,
        "barrier_us is the sum of its three components, never wall time"
    );
    assert!(
        after.store.barrier_fs_batches >= after.store.barrier_fs,
        "a filesystem-wide barrier is shared by at least the batch that ran it"
    );
}

/// The durability policy travels with the counters so nothing compares two
/// runs that did not promise the same thing.
#[test]
fn report_carries_the_catalog_durability_contract() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let report = f.stats_report();
    assert_eq!(report.durability.journal_mode, "wal");
    assert_eq!(report.durability.synchronous, 2);
    assert!(!report.durability.read_only);
}
