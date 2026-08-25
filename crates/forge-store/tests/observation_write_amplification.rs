//! #308: what a read costs the catalog in row mutations.
//!
//! `MetaStats::txn_count` cannot see this: it counts only explicit
//! `BEGIN IMMEDIATE` blocks, so an autocommit `INSERT OR REPLACE` per read
//! reports zero transactions. `Meta::row_mutations` is `sqlite3_total_changes`
//! and counts every row actually written.

use forge_store::{Meta, Observed};
use forge_types::ObjectId;
use tempfile::tempdir;

#[test]
fn rereading_an_unchanged_path_mutates_no_row() {
    let dir = tempdir().unwrap();
    let meta = Meta::open(&dir.path().join("meta.sqlite")).unwrap();
    let oid = ObjectId([7; 32]);

    let start = meta.row_mutations();
    meta.observe("ns", "/", "a.txt", Observed::Blob(oid))
        .unwrap();
    let after_first = meta.row_mutations();
    meta.observe("ns", "/", "a.txt", Observed::Blob(oid))
        .unwrap();
    let after_second = meta.row_mutations();

    assert_eq!(
        after_first - start,
        1,
        "the first read of a path must record exactly one observation row"
    );
    assert_eq!(
        after_second - after_first,
        0,
        "reading the same path again rewrote an already-correct observation row"
    );
    assert_eq!(
        after_second - start,
        1,
        "two reads of one unchanged path must cost exactly one row mutation"
    );

    // I9 is unweakened. The skip is only for a row that is already right.
    let moved = ObjectId([8; 32]);
    meta.observe("ns", "/", "a.txt", Observed::Blob(moved))
        .unwrap();
    assert!(
        meta.row_mutations() > after_second,
        "a changed OID must still be written"
    );
    let rows = meta.observations("ns").unwrap();
    assert_eq!(rows.len(), 1, "one path, one row");
    assert_eq!(rows[0].seen.oid(), Some(moved));

    // A path that disappeared is a different observation, not the same one.
    let before_absent = meta.row_mutations();
    meta.observe("ns", "/", "a.txt", Observed::Absent).unwrap();
    assert!(
        meta.row_mutations() > before_absent,
        "blob -> absent must still be written"
    );
    assert_eq!(meta.observations("ns").unwrap()[0].seen.kind(), "absent");
    let after_absent = meta.row_mutations();
    meta.observe("ns", "/", "a.txt", Observed::Absent).unwrap();
    assert_eq!(
        meta.row_mutations(),
        after_absent,
        "a repeated absent observation rewrote its row"
    );

    // Same path, different mount, is a different row and must be written.
    let before_mount = meta.row_mutations();
    meta.observe("ns", "/other", "a.txt", Observed::Absent)
        .unwrap();
    assert_eq!(meta.row_mutations() - before_mount, 1);
    assert_eq!(meta.observations("ns").unwrap().len(), 2);
}

#[test]
fn a_committed_write_is_visible_to_the_next_read_connection_query() {
    let dir = tempdir().unwrap();
    let meta = Meta::open(&dir.path().join("meta.sqlite")).unwrap();
    let a = ObjectId([1; 32]);
    let b = ObjectId([2; 32]);

    meta.insert_ref("heads/x", a, "commit", false, false, "t", "init")
        .unwrap();
    // `get_ref` runs on a read connection now. A WAL reader sees the last
    // committed state, and the write above committed before this call began.
    assert_eq!(meta.get_ref("heads/x").unwrap().unwrap().oid, a);

    meta.cas_ref("heads/x", a, b, "commit", "t", "t", false)
        .unwrap();
    assert_eq!(
        meta.get_ref("heads/x").unwrap().unwrap().oid,
        b,
        "a read connection served a stale snapshot after a committed CAS"
    );

    meta.insert_namespace("ns", "agent", b, "heads/x").unwrap();
    meta.insert_mount("ns", "/", "ref:heads/x", "rw", Some(b))
        .unwrap();
    assert_eq!(meta.list_mounts("ns").unwrap().len(), 1);
    meta.observe("ns", "/", "a.txt", Observed::Blob(a)).unwrap();
    assert_eq!(meta.observations("ns").unwrap().len(), 1);
    meta.observations_clear("ns").unwrap();
    assert!(
        meta.observations("ns").unwrap().is_empty(),
        "a read connection served observations that were already deleted"
    );
}
