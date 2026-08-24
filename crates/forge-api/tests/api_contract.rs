//! Cross-cutting public API contract. Race, process, and crash proofs remain in
//! their focused test binaries; this matrix keeps the facade's invariant seams
//! visible in one place.

mod support;

use forge_api::{Forge, FsckReport};
use forge_types::{CasResult, Error};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use support::Fixture;
use tempfile::tempdir;

type ContractCase = (&'static str, fn());

#[test]
fn invariant_contract_matrix() {
    let cases: &[ContractCase] = &[
        (
            "I5 protected refs fail closed",
            i5_protected_refs_fail_closed,
        ),
        (
            "I8 sessions read their pinned base",
            i8_sessions_read_their_pinned_base,
        ),
        (
            "I9 stale observations block checkin",
            i9_stale_observations_block_checkin,
        ),
        (
            "I11 overlapping writes are loud",
            i11_overlapping_writes_are_loud,
        ),
        (
            "I13/I14 roles and namespaces carry no ambient authority",
            i13_i14_roles_and_namespaces_have_no_ambient_authority,
        ),
        (
            "I15 sealed releases verify after a durable reopen",
            i15_sealed_releases_verify_after_reopen,
        ),
        (
            "I15/I17 full fsck proves the mutable catalog",
            i15_i17_full_fsck_proves_catalog,
        ),
        (
            "I17 future repository versions fail closed",
            i17_future_versions_fail_closed,
        ),
    ];

    for (name, case) in cases {
        eprintln!("contract: {name}");
        case();
    }
}

fn updated_ref(result: CasResult) -> String {
    match result {
        CasResult::Updated { name, .. } => name,
        other => panic!("expected updated ref, got {other:?}"),
    }
}

fn i5_protected_refs_fail_closed() {
    let fixture = Fixture::new();
    let agent = fixture.agent("writer");
    let ns = fixture.session(&agent, "main");
    fixture
        .forge
        .mount(&agent, &ns, "/", "ref:main", true)
        .unwrap();
    fixture
        .forge
        .write(&agent, &ns, "/forbidden.txt", b"no", false)
        .unwrap();

    let error = fixture
        .forge
        .checkin(&agent, &ns, "/", "must not publish")
        .unwrap_err();
    assert!(matches!(error, Error::Denied(_)), "{error}");
}

fn i8_sessions_read_their_pinned_base() {
    let fixture = Fixture::new();
    fixture
        .forge
        .branch(&fixture.root, "main", "shared")
        .unwrap();

    let seed = fixture.session(&fixture.root, "shared");
    fixture
        .forge
        .mount(&fixture.root, &seed, "/", "ref:shared", true)
        .unwrap();
    fixture
        .forge
        .write(&fixture.root, &seed, "/value.txt", b"v0", false)
        .unwrap();
    updated_ref(
        fixture
            .forge
            .checkin(&fixture.root, &seed, "/", "seed")
            .unwrap(),
    );

    let pinned = fixture.session(&fixture.root, "shared");
    fixture
        .forge
        .mount(&fixture.root, &pinned, "/", "ref:shared", true)
        .unwrap();
    let mover = fixture.session(&fixture.root, "shared");
    fixture
        .forge
        .mount(&fixture.root, &mover, "/", "ref:shared", true)
        .unwrap();
    fixture
        .forge
        .write(&fixture.root, &mover, "/value.txt", b"v1", false)
        .unwrap();
    updated_ref(
        fixture
            .forge
            .checkin(&fixture.root, &mover, "/", "advance")
            .unwrap(),
    );

    assert_eq!(
        fixture
            .forge
            .read(&fixture.root, &pinned, "/value.txt")
            .unwrap(),
        b"v0"
    );
    fixture
        .forge
        .write(&fixture.root, &pinned, "/mine.txt", b"mine", false)
        .unwrap();
    let result = fixture
        .forge
        .checkin(&fixture.root, &pinned, "/", "preserve loser")
        .unwrap();
    assert!(matches!(result, CasResult::Forked { .. }), "{result:?}");
}

fn i9_stale_observations_block_checkin() {
    let fixture = Fixture::new();
    let alice = fixture.agent("alice");
    let bob = fixture.agent("bob");

    let alice_v1 = fixture.session(&alice, "main");
    fixture
        .forge
        .write(&alice, &alice_v1, "/shared.txt", b"v1", false)
        .unwrap();
    let alice_ref = updated_ref(fixture.forge.checkin(&alice, &alice_v1, "/", "v1").unwrap());
    fixture
        .forge
        .merge(&fixture.integrator, "main", &alice_ref, None)
        .unwrap();

    let bob_session = fixture.session(&bob, "main");
    assert_eq!(
        fixture
            .forge
            .read(&bob, &bob_session, "/main/shared.txt")
            .unwrap(),
        b"v1"
    );

    let alice_v2 = fixture.session(&alice, "main");
    fixture
        .forge
        .write(&alice, &alice_v2, "/shared.txt", b"v2", false)
        .unwrap();
    let alice_ref = updated_ref(fixture.forge.checkin(&alice, &alice_v2, "/", "v2").unwrap());
    fixture
        .forge
        .merge(&fixture.integrator, "main", &alice_ref, None)
        .unwrap();

    fixture
        .forge
        .write(&bob, &bob_session, "/notes.txt", b"mine", false)
        .unwrap();
    let error = fixture
        .forge
        .checkin(&bob, &bob_session, "/", "stale")
        .unwrap_err();
    assert!(matches!(error, Error::StaleObservation { .. }), "{error}");
}

fn i11_overlapping_writes_are_loud() {
    let fixture = Fixture::new();
    let left = fixture.session(&fixture.root, "main");
    let right = fixture.session(&fixture.root, "main");
    fixture
        .forge
        .write(&fixture.root, &left, "/same.txt", b"left", false)
        .unwrap();
    fixture
        .forge
        .write(&fixture.root, &right, "/same.txt", b"right", false)
        .unwrap();
    let left_ref = updated_ref(
        fixture
            .forge
            .checkin(&fixture.root, &left, "/", "left")
            .unwrap(),
    );
    let right_ref = updated_ref(
        fixture
            .forge
            .checkin(&fixture.root, &right, "/", "right")
            .unwrap(),
    );
    fixture
        .forge
        .merge(&fixture.integrator, "main", &left_ref, None)
        .unwrap();
    let error = fixture
        .forge
        .merge(&fixture.integrator, "main", &right_ref, None)
        .unwrap_err();
    assert!(matches!(error, Error::MergeConflict(_)), "{error}");
}

fn i13_i14_roles_and_namespaces_have_no_ambient_authority() {
    let fixture = Fixture::new();
    let alice = fixture.agent("alice");
    let bob = fixture.agent("bob");
    let alice_ns = fixture.session(&alice, "main");
    fixture
        .forge
        .write(&alice, &alice_ns, "/secret.txt", b"alice", false)
        .unwrap();

    let namespace_error = fixture
        .forge
        .read(&bob, &alice_ns, "/secret.txt")
        .unwrap_err();
    assert!(
        matches!(namespace_error, Error::Denied(_)),
        "{namespace_error}"
    );
    let role_error = fixture.forge.seal(&alice, "main", "forbidden").unwrap_err();
    assert!(matches!(role_error, Error::Denied(_)), "{role_error}");
}

fn i15_sealed_releases_verify_after_reopen() {
    let fixture = Fixture::new();
    let session = fixture.session(&fixture.root, "main");
    fixture
        .forge
        .write(&fixture.root, &session, "/release.txt", b"final", false)
        .unwrap();
    let contribution = updated_ref(
        fixture
            .forge
            .checkin(&fixture.root, &session, "/", "release")
            .unwrap(),
    );
    fixture
        .forge
        .merge(&fixture.integrator, "main", &contribution, None)
        .unwrap();
    fixture
        .forge
        .seal(&fixture.integrator, "main", "contract-v1")
        .unwrap();

    let path = fixture.path().to_path_buf();
    let root = fixture.root.clone();
    drop(fixture.forge);
    let reopened = Forge::open_read_only(&path).unwrap();
    reopened.verify_tag(&root, "contract-v1").unwrap();
}

fn i17_future_versions_fail_closed() {
    let fixture = Fixture::new();
    let path = fixture.path().to_path_buf();
    drop(fixture.forge);
    fs::write(path.join(".forge/VERSION"), b"2\n").unwrap();
    let error = Forge::open(&path)
        .err()
        .expect("future VERSION must fail closed");
    assert!(matches!(error, Error::Invalid(_)), "{error}");
}

fn edit_catalog(dir: &Path, sql: &str) {
    let conn = Connection::open(dir.join(".forge/meta.sqlite")).unwrap();
    conn.execute_batch(sql).unwrap();
}

fn corrupted_catalog_report(sql: &str) -> FsckReport {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    drop(forge);
    edit_catalog(dir.path(), sql);

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    forge.fsck(&root, true).unwrap()
}

fn finding_resources<'a>(report: &'a FsckReport, code: &str) -> Vec<&'a str> {
    report
        .findings
        .iter()
        .filter(|finding| finding.code == code)
        .map(|finding| finding.resource.as_str())
        .collect()
}

fn i15_i17_full_fsck_proves_catalog() {
    healthy_catalog_is_clean_after_reopen();
    terminal_reflog_must_equal_current_ref();
    reflog_chain_must_be_contiguous();
    reflog_name_must_have_a_current_ref();
    full_fsck_reports_a_damaged_schema_ledger();
    full_fsck_reports_a_missing_schema_ledger();
    malformed_storage_classes_are_findings();
    malformed_oids_are_findings();
    ref_names_retain_their_kind_invariant();
    seal_row_must_agree_with_snapshot_payload();
    seal_row_must_have_its_frozen_tag_ref();
    landmark_oid_must_have_its_declared_type();
    object_intro_value_must_be_a_tree_entry();
    object_intro_commit_must_be_a_commit();
    orphan_mount_is_reported_deterministically();
}

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

    let reopened = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = reopened.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
}

fn terminal_reflog_must_equal_current_ref() {
    let report = corrupted_catalog_report(
        "UPDATE reflog SET new_oid=zeroblob(32)
         WHERE id=(SELECT max(id) FROM reflog WHERE name='main');",
    );
    assert_eq!(
        finding_resources(&report, "REFLOG_TERMINAL"),
        ["catalog:ref:main"]
    );
}

fn reflog_chain_must_be_contiguous() {
    let report = corrupted_catalog_report(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms)
         SELECT name, zeroblob(32), oid, 'test', 'cas', 0
         FROM refs WHERE name='main';",
    );
    assert_eq!(
        finding_resources(&report, "REFLOG_CHAIN"),
        ["catalog:reflog:main:2"]
    );
}

fn reflog_name_must_have_a_current_ref() {
    let report = corrupted_catalog_report(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms)
         SELECT 'ghost', NULL, oid, 'test', 'test', 0
         FROM refs WHERE name='main';",
    );
    assert_eq!(
        finding_resources(&report, "REFLOG_ORPHAN"),
        ["catalog:reflog:ghost"]
    );
}

fn full_fsck_reports_a_damaged_schema_ledger() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    drop(forge);
    edit_catalog(dir.path(), "DELETE FROM schema_migrations;");

    assert!(Forge::open_read_only(dir.path()).is_err());
    assert!(Forge::open_for_fsck(dir.path(), false).is_err());
    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        finding_resources(&report, "SCHEMA_LEDGER"),
        ["catalog:schema_migrations"]
    );
}

fn full_fsck_reports_a_missing_schema_ledger() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    drop(forge);
    edit_catalog(dir.path(), "DROP TABLE schema_migrations;");

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        finding_resources(&report, "SCHEMA_LEDGER"),
        ["catalog:schema_migrations"]
    );
}

fn malformed_storage_classes_are_findings() {
    let report = corrupted_catalog_report(
        "UPDATE refs SET protected='corrupt' WHERE name='main';",
    );
    assert_eq!(
        finding_resources(&report, "CATALOG_VALUE"),
        ["catalog:refs:row:1"]
    );
}

fn malformed_oids_are_findings() {
    let report = corrupted_catalog_report(
        "PRAGMA ignore_check_constraints=ON;
         UPDATE refs SET oid=X'00' WHERE name='main';",
    );
    assert_eq!(
        finding_resources(&report, "CATALOG_OID"),
        ["catalog:ref:main"]
    );
}

fn ref_names_retain_their_kind_invariant() {
    let report = corrupted_catalog_report(
        "UPDATE refs SET kind='snapshot' WHERE name='main';",
    );
    assert_eq!(
        finding_resources(&report, "REF_KIND"),
        ["catalog:ref:main"]
    );
}

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

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        finding_resources(&report, "SEAL_PAYLOAD"),
        ["catalog:seal:catalog-seal"]
    );
}

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

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        finding_resources(&report, "SEAL_TAG_REF"),
        ["catalog:seal:orphan-seal"]
    );
}

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

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    let expected = format!("catalog:landmark:{}", snapshot.hex());
    assert_eq!(
        finding_resources(&report, "TYPE_MISMATCH"),
        [expected.as_str()]
    );
}

fn object_intro_value_must_be_a_tree_entry() {
    let report = corrupted_catalog_report(
        "UPDATE object_intro
         SET oid=(SELECT oid FROM refs WHERE name='main')
         WHERE oid=(SELECT oid FROM object_intro ORDER BY oid LIMIT 1);",
    );
    let resources = finding_resources(&report, "TYPE_MISMATCH");
    assert_eq!(resources.len(), 1, "{:#?}", report.findings);
    assert!(resources[0].starts_with("catalog:object_intro:"));
}

fn object_intro_commit_must_be_a_commit() {
    let report = corrupted_catalog_report(
        "UPDATE object_intro SET commit_oid=oid
         WHERE oid=(SELECT oid FROM object_intro ORDER BY oid LIMIT 1);",
    );
    let resources = finding_resources(&report, "TYPE_MISMATCH");
    assert_eq!(resources.len(), 1, "{:#?}", report.findings);
    assert!(resources[0].starts_with("catalog:object_intro:"));
    assert!(resources[0].ends_with(":commit"));
}

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

    let forge = Forge::open_for_fsck(dir.path(), true).unwrap();
    let report = forge.fsck(&root, true).unwrap();
    assert_eq!(
        finding_resources(&report, "MOUNT_NAMESPACE"),
        ["catalog:mount:ghost:/"]
    );
}
