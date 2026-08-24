//! Cross-cutting public API contract. Race, process, and crash proofs remain in
//! their focused test binaries; this matrix keeps the facade's invariant seams
//! visible in one place.

mod support;

use forge_api::Forge;
use forge_types::{CasResult, Error};
use std::fs;
use support::Fixture;

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
