//! I15/I17: full fsck proves the mutable catalog as well as immutable object
//! closure. Each fixture introduces one surgical relational defect and checks
//! its stable finding code.

use forge_api::{Forge, FsckReport};
use rusqlite::Connection;
use std::path::Path;
use tempfile::tempdir;

fn edit_catalog(dir: &Path, sql: &str) {
    let conn = Connection::open(dir.join(".forge/meta.sqlite")).unwrap();
    conn.execute_batch(sql).unwrap();
}

fn corrupted_report(sql: &str) -> FsckReport {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    drop(forge);
    edit_catalog(dir.path(), sql);

    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    forge.fsck(&root, true).unwrap()
}

fn findings<'a>(report: &'a FsckReport, code: &str) -> Vec<&'a str> {
    report
        .findings
        .iter()
        .filter(|finding| finding.code == code)
        .map(|finding| finding.resource.as_str())
        .collect()
}

#[test]
fn healthy_catalog_is_clean_after_reopen() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();

    let ns = forge.session_open(&root, "main").unwrap();
    forge
        .write(&root, &ns, "/kept.txt", b"kept", false)
        .unwrap();
    forge.checkin(&root, &ns, "/", "catalog fixture").unwrap();
    forge.seal(&root, "main", "catalog-clean").unwrap();
    drop(forge);

    let reopened = Forge::open_for_fsck(dir.path()).unwrap();
    let report = reopened.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
}

#[test]
fn terminal_reflog_must_equal_current_ref() {
    let report = corrupted_report(
        "UPDATE reflog SET new_oid=zeroblob(32)
         WHERE id=(SELECT max(id) FROM reflog WHERE name='main');",
    );
    assert_eq!(findings(&report, "REFLOG_TERMINAL"), ["catalog:ref:main"]);
}

#[test]
fn reflog_chain_must_be_contiguous() {
    let report = corrupted_report(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms)
         SELECT name, zeroblob(32), oid, 'test', 'cas', 0
         FROM refs WHERE name='main';",
    );
    assert_eq!(findings(&report, "REFLOG_CHAIN"), ["catalog:reflog:main:2"]);
}

#[test]
fn reflog_name_must_have_a_current_ref() {
    let report = corrupted_report(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms)
         SELECT 'ghost', NULL, oid, 'test', 'test', 0
         FROM refs WHERE name='main';",
    );
    assert_eq!(findings(&report, "REFLOG_ORPHAN"), ["catalog:reflog:ghost"]);
}

#[test]
fn fsck_opens_and_reports_a_damaged_schema_ledger() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    drop(forge);
    edit_catalog(dir.path(), "DELETE FROM schema_migrations;");

    assert!(Forge::open_read_only(dir.path()).is_err());
    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        findings(&report, "SCHEMA_LEDGER"),
        ["catalog:schema_migrations"]
    );
}

#[test]
fn seal_row_must_agree_with_snapshot_payload() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "catalog-seal").unwrap();
    drop(forge);
    edit_catalog(
        dir.path(),
        "UPDATE seals SET tree_oid=commit_oid WHERE tag='catalog-seal';",
    );

    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        findings(&report, "SEAL_PAYLOAD"),
        ["catalog:seal:catalog-seal"]
    );
}

#[test]
fn seal_row_must_have_its_frozen_tag_ref() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "orphan-seal").unwrap();
    drop(forge);
    edit_catalog(
        dir.path(),
        "DELETE FROM refs WHERE name='tags/orphan-seal';",
    );

    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        findings(&report, "SEAL_TAG_REF"),
        ["catalog:seal:orphan-seal"]
    );
}

#[test]
fn landmark_oid_must_have_its_declared_type() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let snapshot = forge.seal(&root, "main", "landmark-type").unwrap();
    drop(forge);
    edit_catalog(
        dir.path(),
        &format!(
            "UPDATE landmarks SET kind='blob' WHERE oid=X'{}';",
            snapshot.hex()
        ),
    );

    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    let expected = format!("catalog:landmark:{}", snapshot.hex());
    assert_eq!(findings(&report, "TYPE_MISMATCH"), [expected.as_str()]);
}

#[test]
fn object_intro_value_must_be_a_tree_entry() {
    let report = corrupted_report(
        "UPDATE object_intro
         SET oid=(SELECT oid FROM refs WHERE name='main')
         WHERE oid=(SELECT oid FROM object_intro ORDER BY oid LIMIT 1);",
    );
    let resources = findings(&report, "TYPE_MISMATCH");
    assert_eq!(resources.len(), 1, "{:#?}", report.findings);
    assert!(resources[0].starts_with("catalog:object_intro:"));
}

#[test]
fn object_intro_commit_must_be_a_commit() {
    let report = corrupted_report(
        "UPDATE object_intro SET commit_oid=oid
         WHERE oid=(SELECT oid FROM object_intro ORDER BY oid LIMIT 1);",
    );
    let resources = findings(&report, "TYPE_MISMATCH");
    assert_eq!(resources.len(), 1, "{:#?}", report.findings);
    assert!(resources[0].starts_with("catalog:object_intro:"));
    assert!(resources[0].ends_with(":commit"));
}

#[test]
fn orphan_mount_is_reported_deterministically() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    drop(forge);
    edit_catalog(
        dir.path(),
        &format!("UPDATE mounts SET ns_id='ghost' WHERE ns_id='{ns}' AND path='/';"),
    );

    let forge = Forge::open_for_fsck(dir.path()).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        findings(&report, "MOUNT_NAMESPACE"),
        ["catalog:mount:ghost:/"]
    );
}
