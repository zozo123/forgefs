//! Issue #39 regression: native rename plus exact-identity conflict detection.
//!
//! Publication and staging are atomic through Forge::rename (I24). Merge-time
//! detection now compares the immutable base/ours/theirs trees and infers only
//! one-to-one relocations of a unique full entry identity (oid, kind, exec).
//!
//! Measured contract (see each test):
//!
//! * rename-versus-edit remains a Conflict and preserves both refs;
//! * divergent exact renames and exact rename-versus-delete are now conflicts;
//! * two agents choosing the same destination converge without a conflict;
//! * a copy whose source remains is not guessed to be a move;
//! * FORMAT v1 Contribution.writes remains a flat path list. Rename conflict
//!   detection is derived from trees and adds no new trusted metadata.

use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

fn ref_of(result: &CasResult) -> String {
    match result {
        CasResult::Updated { name, .. } | CasResult::Noop { name, .. } => name.clone(),
        CasResult::Forked { fork, .. } => fork.clone(),
    }
}

/// Flat `path -> content` view of a ref, so outcomes are compared by content
/// and never by commit oid (commits embed a timestamp and a message).
fn listing(forge: &Forge, cap: &Cap, r#ref: &str) -> Vec<(String, String)> {
    let ns = forge.session_open(cap, r#ref).unwrap();
    let mut out = Vec::new();
    for (name, kind, _, _) in forge.ls(cap, &ns, "/").unwrap() {
        assert_eq!(kind, "blob", "these fixtures are flat");
        let data = forge.read(cap, &ns, &format!("/{name}")).unwrap();
        out.push((name, String::from_utf8(data).unwrap()));
    }
    out
}

fn seeded() -> (tempfile::TempDir, Forge, Cap, Cap) {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let integrator = forge.integrator_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge.write(&root, &ns, "/x", b"v1", false).unwrap();
    let seed = forge.checkin(&root, &ns, "/", "seed").unwrap();
    forge
        .merge(&integrator, "main", &ref_of(&seed), None)
        .unwrap();
    (d, forge, root, integrator)
}

/// Use the native one-transaction staging operation from I24.
fn rename(forge: &Forge, cap: &Cap, ns: &str, from: &str, to: &str) {
    forge.rename(cap, ns, from, to, None).unwrap();
}

/// I11/I18: agent A renames /x to /y while agent B edits /x. Integration of the
/// second contribution must be a Conflict object and must leave both the
/// destination ref and B's work intact. This is the case issue #39 calls a
/// silent loss; it is not silent, and this test is what keeps it that way.
#[test]
fn i11_rename_versus_concurrent_edit_is_a_conflict_not_a_silent_loss() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();

    rename(&forge, &root, &a, "/x", "/y");
    forge.write(&root, &b, "/x", b"v2", false).unwrap();

    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename /x to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "edit /x").unwrap());

    forge.merge(&integrator, "main", &ra, None).unwrap();
    let before = forge.peel_commit("main").unwrap().0;

    let err = forge
        .merge(&integrator, "main", &rb, None)
        .expect_err("an edit to a renamed path must not merge away silently");
    let Error::MergeConflict(conflict_oid) = err else {
        panic!("expected a Conflict object, got {err:?}");
    };
    assert_eq!(
        Error::MergeConflict(conflict_oid).code(),
        "conflict",
        "CLI_ABI maps this to exit 4"
    );

    // The Conflict is durable, typed, and names the renamed-away path.
    let shown = forge.show(&root, &format!("oid:{conflict_oid}")).unwrap();
    assert!(shown.contains(" x"), "conflict must name the path: {shown}");
    let conflict_refs = forge
        .refs(&root)
        .unwrap()
        .into_iter()
        .filter(|r| r.name.starts_with("conflicts/main/"))
        .count();
    assert_eq!(conflict_refs, 1, "a conflict ref must be published");

    // I5/I18: the refused merge moved nothing and destroyed nothing.
    assert_eq!(forge.peel_commit("main").unwrap().0, before);
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into())]
    );
    assert_eq!(listing(&forge, &root, &rb), vec![("x".into(), "v2".into())]);
}

/// The same pair integrated in the other order is equally loud: I12 says the
/// outcome comes from the DAG, not from who committed first.
#[test]
fn i12_rename_versus_concurrent_edit_conflicts_in_either_integration_order() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    forge.write(&root, &b, "/x", b"v2", false).unwrap();
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename /x to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "edit /x").unwrap());

    forge.merge(&integrator, "main", &rb, None).unwrap();
    let err = forge.merge(&integrator, "main", &ra, None).unwrap_err();
    assert!(
        matches!(err, Error::MergeConflict(_)),
        "reversed order must also conflict, got {err:?}"
    );
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("x".into(), "v2".into())]
    );
}

/// FORMAT v1 receipts still list touched paths rather than encoding a move.
/// That no longer makes exact conflict detection impossible: the merge derives
/// unambiguous relocations from the three immutable trees.
#[test]
fn i10_rename_provenance_cannot_express_the_move() {
    let (_d, forge, root, _integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    let CasResult::Updated { oid, .. } = forge.checkin(&root, &a, "/", "rename /x to /y").unwrap()
    else {
        panic!("private checkin must update the session ref");
    };
    let (_, commit) = forge.peel_commit(&format!("oid:{oid}")).unwrap();
    let contrib = commit.contrib.expect("I10: material checkin has a receipt");
    let shown = forge.show(&root, &format!("oid:{contrib}")).unwrap();

    assert!(
        shown.contains("write /x"),
        "deleted path listed as a write: {shown}"
    );
    assert!(
        shown.contains("write /y"),
        "new path listed as a write: {shown}"
    );
    for word in ["rename", "move", "delete", "from "] {
        assert!(
            !shown.contains(word),
            "receipt unexpectedly encodes {word:?}; issue #39 assumes it cannot: {shown}"
        );
    }
}

/// Two agents move the same unique object to different destinations. Exact
/// identity detection must make the second integration loud and retain both
/// destination trees.
#[test]
fn divergent_exact_renames_are_a_conflict() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    rename(&forge, &root, &b, "/x", "/z");
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "rename to /z").unwrap());

    forge.merge(&integrator, "main", &ra, None).unwrap();
    let err = forge
        .merge(&integrator, "main", &rb, None)
        .expect_err("divergent exact renames must conflict");
    assert!(matches!(err, Error::MergeConflict(_)), "{err:?}");
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into())]
    );
    assert_eq!(listing(&forge, &root, &rb), vec![("z".into(), "v1".into())]);
}

#[test]
fn matching_exact_renames_converge_without_a_conflict() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    rename(&forge, &root, &b, "/x", "/y");
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "same rename").unwrap());

    forge.merge(&integrator, "main", &ra, None).unwrap();
    forge.merge(&integrator, "main", &rb, None).unwrap();
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into())]
    );
}

/// An exact rename versus a delete is a semantic disagreement even though both
/// sides remove the source path. The retained destination must not make the
/// delete disappear silently.
#[test]
fn exact_rename_versus_delete_is_a_conflict() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    forge.delete(&root, &b, "/x").unwrap();
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "delete /x").unwrap());

    forge.merge(&integrator, "main", &ra, None).unwrap();
    let err = forge
        .merge(&integrator, "main", &rb, None)
        .expect_err("rename versus delete must conflict");
    assert!(matches!(err, Error::MergeConflict(_)), "{err:?}");
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into())]
    );
    assert!(listing(&forge, &root, &rb).is_empty());
}

/// A copy is deliberately not a rename candidate while its source remains.
/// The concurrent edit lands on the old path and the copy keeps the original
/// bytes: following copies would be a content heuristic, not exact relocation.
#[test]
fn copy_with_live_source_is_not_guessed_to_be_a_rename() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    let data = forge.read(&root, &a, "/x").unwrap();
    forge.write(&root, &a, "/y", &data, false).unwrap();
    forge.write(&root, &b, "/x", b"v2", false).unwrap();
    let ra = ref_of(&forge.checkin(&root, &a, "/", "copy /x to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "edit /x").unwrap());
    forge.merge(&integrator, "main", &ra, None).unwrap();
    forge.merge(&integrator, "main", &rb, None).unwrap();
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("x".into(), "v2".into()), ("y".into(), "v1".into())],
        "exact relocation must not turn an ordinary copy into a move"
    );
}
