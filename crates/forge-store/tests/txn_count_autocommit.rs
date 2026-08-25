//! Issue #311: `MetaStats::txn_count` must not be blind to autocommit writes.
//!
//! Under the old counter only the four explicit `BEGIN IMMEDIATE` sites were
//! instrumented, so a session that only *read* -- and therefore only wrote
//! observation rows through autocommit `INSERT OR REPLACE` -- reported
//! `"txn": 0` from `forge stats --json` while SQLite committed one write
//! transaction per read. Every CI check or dashboard keyed on that number
//! under-reported the catalog's write traffic by the whole read-heavy phase.

use forge_store::{Meta, Observed};
use forge_types::ObjectId;
use tempfile::tempdir;

fn meta() -> (tempfile::TempDir, Meta) {
    let dir = tempdir().expect("tempdir");
    let meta = Meta::open(&dir.path().join("meta.sqlite")).expect("open catalog");
    (dir, meta)
}

/// The regression itself: reads only, no explicit transaction, and the write
/// transactions they cost must still be reported.
#[test]
fn a_read_heavy_phase_reports_the_write_transactions_it_ran() {
    let (_dir, meta) = meta();
    let oid = ObjectId([7; 32]);

    let opened = meta.stats();
    assert_eq!(
        opened.txn_count, 0,
        "schema setup during open is not caller work and must stay out of the counter"
    );
    // `row_mutations` is `sqlite3_total_changes` for the whole connection, so
    // it already carries the rows open wrote. Only the delta is this phase.
    let rows_at_open = meta.row_mutations();

    const READS: u64 = 64;
    for i in 0..READS {
        meta.observe("ns", "/", &format!("f{i}.txt"), Observed::Blob(oid))
            .expect("observe");
    }

    let after = meta.stats();
    assert_eq!(
        after.explicit_txn_count, 0,
        "observe() is autocommit; it opens no explicit transaction"
    );
    assert_eq!(
        after.txn_count, READS,
        "#311: {READS} autocommit observation writes must count as {READS} write transactions, \
         not as zero"
    );
    assert_eq!(
        meta.row_mutations() - rows_at_open,
        READS,
        "one observation row per read, as a cross-check on the transaction count"
    );
}

/// The counter must count transactions, not statements offered to SQLite. The
/// #315 no-op skip means a repeated identical observation issues no DML at
/// all, so nothing is committed and nothing may be counted.
#[test]
fn a_skipped_no_op_observation_commits_nothing_and_counts_nothing() {
    let (_dir, meta) = meta();
    let oid = ObjectId([9; 32]);

    meta.observe("ns", "/", "a.txt", Observed::Blob(oid))
        .expect("first observe");
    let after_first = meta.stats().txn_count;
    assert_eq!(after_first, 1);

    for _ in 0..16 {
        meta.observe("ns", "/", "a.txt", Observed::Blob(oid))
            .expect("repeat observe");
    }
    assert_eq!(
        meta.stats().txn_count,
        after_first,
        "an observation that rewrote nothing must not be counted as a write transaction"
    );
}

/// An explicit transaction is one transaction however many statements it
/// carries, and a refused one committed nothing. Without this the fix could
/// degenerate into counting statements.
#[test]
fn an_explicit_transaction_counts_once_and_a_refused_one_not_at_all() {
    let (_dir, meta) = meta();
    let a = ObjectId([1; 32]);
    let b = ObjectId([2; 32]);
    let rows_at_open = meta.row_mutations();

    // insert_ref is one explicit transaction carrying a refs row plus a
    // reflog row.
    meta.insert_ref("heads/x", a, "commit", false, false, "t", "init")
        .expect("insert_ref");
    let after_insert = meta.stats();
    assert_eq!(after_insert.txn_count, 1, "two rows, one transaction");
    assert_eq!(after_insert.explicit_txn_count, 1);
    assert_eq!(
        meta.row_mutations() - rows_at_open,
        2,
        "two rows were in fact written"
    );

    // A protected ref refuses the CAS. The attempt is timed, but nothing was
    // committed, so no write transaction happened.
    meta.insert_ref("heads/protected", a, "commit", true, false, "t", "init")
        .expect("insert protected");
    let before_denied = meta.stats();
    meta.cas_ref("heads/protected", a, b, "commit", "t", "t", false)
        .expect_err("a protected ref must refuse the CAS");
    let after_denied = meta.stats();
    assert_eq!(
        after_denied.txn_count, before_denied.txn_count,
        "a refused CAS committed nothing and must not be counted as a write transaction"
    );
    assert_eq!(
        after_denied.explicit_txn_count,
        before_denied.explicit_txn_count + 1,
        "the refused attempt is still an explicit attempt, and is still timed"
    );
}

/// The two counters answer different questions, and the invariant between them
/// is one-directional: every explicit transaction that committed is a write
/// transaction, but a write transaction need not be explicit.
#[test]
fn a_mixed_phase_separates_explicit_transactions_from_autocommit_writes() {
    let (_dir, meta) = meta();
    let a = ObjectId([1; 32]);
    let b = ObjectId([2; 32]);

    meta.insert_ref("heads/x", a, "commit", false, false, "t", "init")
        .expect("insert_ref");
    meta.cas_ref("heads/x", a, b, "commit", "t", "t", false)
        .expect("cas_ref");
    for i in 0..8 {
        meta.observe("ns", "/", &format!("f{i}.txt"), Observed::Blob(a))
            .expect("observe");
    }

    let stats = meta.stats();
    assert_eq!(stats.explicit_txn_count, 2, "insert_ref and cas_ref");
    assert_eq!(
        stats.txn_count, 10,
        "two explicit transactions plus eight autocommit observation writes"
    );
    assert!(
        stats.txn_count > stats.explicit_txn_count,
        "the autocommit writes this phase performed are exactly what #311 lost"
    );
}
