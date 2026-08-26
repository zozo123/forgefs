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

/// #42 coverage: the families the issue names -- object bytes, cache hits,
/// merge-base search, GC and fsck -- must each move for the operation that
/// causes them, and must NOT move for one that does not.
#[test]
fn counters_move_for_bytes_caches_merge_base_gc_and_fsck() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();

    let before = f.stats_report();

    let ns = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &ns, "/", "ref:shared", true).unwrap();
    f.write(&root, &ns, "/a.txt", b"payload-bytes", false)
        .unwrap();
    f.checkin(&root, &ns, "/", "one").unwrap();

    let published = f.stats_report();
    assert!(
        published.store.put_bytes > before.store.put_bytes,
        "publishing objects must move the byte counter, not only `puts`"
    );

    // A second namespace republishing identical bytes: a dedup hit, and the
    // bytes it did not have to write.
    let twin = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &twin, "/", "ref:shared", true).unwrap();
    f.write(&root, &twin, "/b.txt", b"payload-bytes", false)
        .unwrap();
    f.checkin(&root, &twin, "/", "two").unwrap();
    let deduped = f.stats_report();
    assert!(
        deduped.store.dedup_bytes > published.store.dedup_bytes,
        "a dedup hit must record the bytes content addressing avoided writing"
    );

    // Reads: the first resolves through the object plane, the rest are served
    // from the hot caches, which is exactly why physical bytes and cache hits
    // are separate numbers.
    let reader = f.session_open(&root, "shared").unwrap();
    for _ in 0..4 {
        f.read(&root, &reader, "/a.txt").unwrap();
    }
    let read = f.stats_report();
    assert!(
        read.cache.object_cache_hits > deduped.cache.object_cache_hits
            || read.cache.tree_cache_hits > deduped.cache.tree_cache_hits,
        "repeated reads of one path must be observable as cache hits"
    );
    assert!(
        read.store.get_bytes >= deduped.store.get_bytes,
        "physical read volume is monotonic"
    );
    assert_eq!(
        read.store.hash_failures, 0,
        "an intact repository must report no hash failure"
    );

    // Merge-base search is timed whatever the merge decides.
    f.branch(&root, "shared", "topic").unwrap();
    let topic = f.session_open(&root, "topic").unwrap();
    f.mount(&root, &topic, "/", "ref:topic", true).unwrap();
    f.write(&root, &topic, "/c.txt", b"topic", false).unwrap();
    f.checkin(&root, &topic, "/", "topic").unwrap();
    f.merge(&root, "shared", "topic", None).unwrap();
    let merged = f.stats_report();
    assert!(
        merged.api.merge_base_searches > read.api.merge_base_searches,
        "a merge must record that it searched for a base"
    );

    // fsck: runs move, findings do not, which is what separates "not checked"
    // from "checked and clean".
    let clean = f.fsck(&root, true).unwrap();
    assert!(clean.ok);
    let checked = f.stats_report();
    assert_eq!(
        checked.api.fsck_runs - merged.api.fsck_runs,
        1,
        "every fsck that produced a report must be counted once"
    );
    assert_eq!(
        checked.api.fsck_findings, merged.api.fsck_findings,
        "a clean fsck must not invent findings"
    );

    // gc: a DRY RUN is a run that deletes nothing. A counter that could not
    // say that would be worse than no counter (I23).
    f.gc(&root, true, forge_api::DEFAULT_MIN_AGE_SECS).unwrap();
    let planned = f.stats_report();
    assert_eq!(planned.api.gc_runs - checked.api.gc_runs, 1);
    assert_eq!(
        planned.api.gc_bytes_deleted, checked.api.gc_bytes_deleted,
        "a dry run deletes nothing and must add nothing to the deleted bytes"
    );
    assert_eq!(
        planned.api.gc_objects_deleted, checked.api.gc_objects_deleted,
        "a dry run deletes nothing and must add nothing to the deleted objects"
    );

    // rename: one move, one count, whatever it moved.
    let mover = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &mover, "/", "ref:shared", true).unwrap();
    f.rename(&root, &mover, "/a.txt", "/moved.txt", None)
        .unwrap();
    let moved = f.stats_report();
    assert_eq!(
        moved.api.renames - planned.api.renames,
        1,
        "a move is counted once, not once per file it staged"
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
