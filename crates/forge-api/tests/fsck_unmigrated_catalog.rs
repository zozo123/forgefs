//! Issue #348: `fsck --full` called an intact, un-migrated repository corrupt.
//!
//! Exit 2 is reserved by CLI_ABI.md for corruption, and "run fsck before you
//! upgrade" is exactly what a careful operator does. A repository written by
//! v0.2.1 -- schema version 2, every object file byte-identical to what the
//! migration would leave behind -- is not corrupt, so `fsck --full` must not
//! say it is. The read-only path already had the right shape: `verify` and
//! reachable `fsck` refuse with `Error::Invalid` (exit 1) naming the version.
//!
//! These tests pin every half of the contract: an older catalog is refused
//! rather than condemned and is not migrated behind the operator's back; a
//! newer one is refused the same way; a genuinely damaged ledger is still
//! reported as the corruption it is, because the ledger-deferred fsck open
//! exists for exactly that case; and one read-write open makes fsck clean.

use forge_api::Forge;
use forge_cap::Cap;
use forge_store::CURRENT_SCHEMA_VERSION;
use forge_types::Error;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// The `mounts` relation exactly as schema version 2 defined it, before v3
/// added the `base_oid` pin. Recreating it is what makes this fixture a real
/// v2 catalog rather than a v3 catalog with a shortened ledger: the audit sees
/// the older table shape too, and reported it as `CATALOG_SCHEMA` corruption
/// alongside the short ledger.
const DOWNGRADE_TO_V2: &str = "\
CREATE TABLE mounts_v2 (
  ns_id TEXT NOT NULL,
  path  TEXT NOT NULL,
  spec  TEXT NOT NULL,
  mode  TEXT NOT NULL CHECK(mode IN ('ro','rw')),
  PRIMARY KEY (ns_id, path)
);
INSERT INTO mounts_v2 (ns_id, path, spec, mode)
  SELECT ns_id, path, spec, mode FROM mounts;
DROP TABLE mounts;
ALTER TABLE mounts_v2 RENAME TO mounts;
DELETE FROM schema_migrations WHERE version > 2;";

fn edit_catalog(root: &Path, sql: &str) {
    let conn = Connection::open(root.join("meta.sqlite")).expect("open catalog");
    conn.execute_batch(sql).expect("apply fixture sql");
    drop(conn);
}

fn ledger(root: &Path) -> Vec<i64> {
    Connection::open(root.join("meta.sqlite"))
        .expect("open catalog")
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("ledger rows")
}

/// Every immutable object byte, keyed by its repository-relative path. Path
/// and content are both content-addressed, so any rewrite shows up here.
fn object_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let objects = root.join("objects");
    let mut found = Vec::new();
    let mut stack = vec![objects.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read objects dir") {
            let entry = entry.expect("objects dir entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                stack.push(path);
            } else {
                let key = path
                    .strip_prefix(&objects)
                    .expect("object path is under objects/")
                    .display()
                    .to_string();
                found.push((key, fs::read(&path).expect("read object")));
            }
        }
    }
    assert!(
        !found.is_empty(),
        "the fixture repository must have objects"
    );
    found.sort();
    found
}

/// A repository with real objects, a real session and a real mount row, so the
/// fsck that follows has something to check and cannot pass vacuously.
fn populated_repo(dir: &Path) -> (PathBuf, Cap) {
    let forge = Forge::init(dir).expect("init");
    let cap = forge.root_cap().expect("root cap");
    let ns = forge.session_open(&cap, "main").expect("session open");
    forge
        .write(&cap, &ns, "/kept.txt", b"kept", false)
        .expect("write");
    forge.checkin(&cap, &ns, "/", "fixture").expect("checkin");
    let root = forge.root().to_path_buf();
    drop(forge);
    (root, cap)
}

#[test]
fn full_fsck_refuses_an_unmigrated_catalog_instead_of_calling_it_corrupt() {
    let dir = tempdir().unwrap();
    let (root, cap) = populated_repo(dir.path());
    edit_catalog(&root, DOWNGRADE_TO_V2);
    assert_eq!(
        ledger(&root),
        vec![1, 2],
        "the fixture must be a v2 catalog"
    );
    let catalog_before = fs::read(root.join("meta.sqlite")).expect("read catalog");

    let forge = Forge::open_for_fsck(dir.path(), true).expect("fsck must still open");
    let error = forge
        .fsck(&cap, true)
        .expect_err("this binary cannot audit a v2 catalog");

    // Exit 1, never exit 2. `Error::Invalid` is the CLI's input family and
    // `Error::Corrupt` is the corruption family; an intact older repository
    // belongs to neither corruption nor success.
    let text = error.to_string();
    assert!(
        matches!(error, Error::Invalid(_)),
        "an intact older repository must not be reported as corruption: {text}"
    );
    assert!(
        text.contains("schema version 2")
            && text.contains(&CURRENT_SCHEMA_VERSION.to_string())
            && text.contains("migrat"),
        "the diagnostic must name the schema version and the remedy: {text}"
    );

    // A tool called to diagnose must not rewrite what it was asked to look at.
    drop(forge);
    assert_eq!(
        fs::read(root.join("meta.sqlite")).expect("read catalog"),
        catalog_before,
        "a refused fsck must leave the catalog byte-identical"
    );
    assert_eq!(ledger(&root), vec![1, 2]);
}

#[test]
fn one_read_write_open_migrates_and_then_full_fsck_is_clean() {
    let dir = tempdir().unwrap();
    let (root, _) = populated_repo(dir.path());
    let objects_before = object_bytes(&root);
    edit_catalog(&root, DOWNGRADE_TO_V2);

    let forge = Forge::open(dir.path()).expect("a v2 catalog migrates on a writable open");
    let cap = forge.root_cap().expect("root cap");
    let report = forge.fsck(&cap, true).expect("fsck after migration");
    assert!(report.ok, "{:#?}", report.findings);
    assert!(report.checked_objects > 0);
    drop(forge);

    assert_eq!(
        ledger(&root),
        (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>()
    );
    assert_eq!(
        object_bytes(&root),
        objects_before,
        "the migration must not touch a single immutable object"
    );
}

/// A ledger with a hole in it is damage, not age, and the ledger-deferred fsck
/// open exists so that damage can be reported rather than refused. The #348 fix
/// must not swallow it.
#[test]
fn full_fsck_still_reports_a_damaged_ledger_as_corruption() {
    let dir = tempdir().unwrap();
    let (root, cap) = populated_repo(dir.path());
    edit_catalog(&root, "DELETE FROM schema_migrations WHERE version = 2;");
    assert_eq!(ledger(&root), vec![1, 3], "the fixture must have a hole");

    let forge = Forge::open_for_fsck(dir.path(), true).expect("fsck must open");
    let report = forge
        .fsck(&cap, true)
        .expect("a damaged ledger is a finding, not a refusal");
    assert!(!report.ok);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.code == "SCHEMA_LEDGER"),
        "{:#?}",
        report.findings
    );
}

/// The same defect in the other direction: a catalog written by a newer
/// ForgeFS is intact, merely unreadable by this binary's auditor, which knows
/// only its own table shapes. Every normal open already refuses it with exit 1.
#[test]
fn full_fsck_refuses_a_newer_catalog_instead_of_calling_it_corrupt() {
    let dir = tempdir().unwrap();
    let (root, cap) = populated_repo(dir.path());
    edit_catalog(
        &root,
        &format!(
            "INSERT INTO schema_migrations (version, applied_ms) VALUES ({}, 0);",
            CURRENT_SCHEMA_VERSION + 1
        ),
    );

    let forge = Forge::open_for_fsck(dir.path(), true).expect("fsck must still open");
    let error = forge.fsck(&cap, true).expect_err("not auditable");
    let text = error.to_string();
    assert!(
        matches!(error, Error::Invalid(_)),
        "a newer repository must not be reported as corruption: {text}"
    );
    assert!(
        text.contains("newer than supported"),
        "the diagnostic must name the incompatibility: {text}"
    );
}
