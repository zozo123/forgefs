//! Issue #39 characterisation: ForgeFS has no native rename.
//!
//! A rename is expressed as `write(new)` + `delete(old)` inside one session
//! overlay, so it is atomic at the *publication* point (one Contribution, one
//! Commit, one CAS) but carries no identity linking the two paths. These tests
//! pin what today's merge and provenance actually do, so that any future work
//! on renames has to change a recorded outcome rather than a belief.
//!
//! Measured summary (see each test):
//!
//! * I11 holds for the case the issue names: a concurrent edit to a renamed
//!   path is a Conflict object and exit-code-4 merge failure, not a silent
//!   loss, and the losing agent's work stays reachable on its own ref (I18).
//! * Three shapes are genuinely silent today, and every one of them needs
//!   rename identity - i.e. a frozen-format change - to become loud:
//!   divergent rename (duplicate), rename vs delete (deletion ignored), and
//!   the copy-only rename the CLI can actually express (stale duplicate).
//! * A rename receipt is byte-indistinguishable from "wrote both paths":
//!   `Contribution.writes` is a flat path list with no add/delete/move tag.

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

/// The rename half of a session overlay: copy the bytes to the new path and
/// tombstone the old one. There is no other way to say it.
fn rename(forge: &Forge, cap: &Cap, ns: &str, from: &str, to: &str) {
    let data = forge.read(cap, ns, from).unwrap();
    forge.write(cap, ns, to, &data, false).unwrap();
    forge.delete(cap, ns, from).unwrap();
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

/// A rename receipt cannot be distinguished from "this agent wrote both
/// paths". I10 receipts carry `writes` as a flat path list, so the tombstone
/// and the new path are reported identically and the move is not provenance.
/// A native rename would have to change this encoding, which FORMAT.md freezes.
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

/// SILENT TODAY. Two agents rename the same file to different names. Nothing
/// overlaps by path, so integration succeeds and the repository ends up with
/// two copies of the same blob and no record that either was a move.
#[test]
fn divergent_rename_duplicates_content_with_no_conflict() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    rename(&forge, &root, &b, "/x", "/z");
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "rename to /z").unwrap());
    forge.merge(&integrator, "main", &ra, None).unwrap();
    forge.merge(&integrator, "main", &rb, None).unwrap();
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into()), ("z".into(), "v1".into())],
        "path-granular merge cannot see that both sides moved the same file"
    );
}

/// SILENT TODAY. A renames /x, B deletes /x. Both sides tombstone /x, so the
/// merge sees agreement on /x and a pure addition at /y: B's deletion is
/// dropped without a Conflict object.
#[test]
fn rename_versus_delete_keeps_the_content_with_no_conflict() {
    let (_d, forge, root, integrator) = seeded();
    let a = forge.session_open(&root, "main").unwrap();
    let b = forge.session_open(&root, "main").unwrap();
    rename(&forge, &root, &a, "/x", "/y");
    forge.delete(&root, &b, "/x").unwrap();
    let ra = ref_of(&forge.checkin(&root, &a, "/", "rename to /y").unwrap());
    let rb = ref_of(&forge.checkin(&root, &b, "/", "delete /x").unwrap());
    forge.merge(&integrator, "main", &ra, None).unwrap();
    forge.merge(&integrator, "main", &rb, None).unwrap();
    assert_eq!(
        listing(&forge, &root, "main"),
        vec![("y".into(), "v1".into())],
        "B's deletion is silently ignored because the move has no identity"
    );
}

/// SILENT TODAY, and this is the only rename the CLI can express: `forge` has
/// no delete verb, so a CLI "rename" is a copy. The concurrent edit lands on
/// the old path, the copy keeps stale bytes forever, and the merge is clean.
#[test]
fn copy_only_rename_leaves_stale_bytes_with_no_conflict() {
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
        "the edit does not follow the copy; nothing reports that /y is stale"
    );
}
