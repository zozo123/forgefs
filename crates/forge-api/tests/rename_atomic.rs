//! I24: `rename` is one staged transaction over one mount (#39).
//!
//! The crash half of this invariant lives in `cli_mv_crash.rs`, which needs
//! real processes. This file pins everything a single process can decide: what
//! a move stages, what it refuses, what it records under I9, and that it adds
//! no commit point of its own -- publication is still one Contribution and one
//! CAS (I5, I10, I19).
//!
//! What this does NOT claim: rename identity. `Contribution.writes` is a flat
//! path list in the frozen VERSION 1 encoding, so a published move is still
//! byte-indistinguishable from "wrote the destination, deleted the source", and
//! merge is still path-granular. `rename_characterisation.rs` records exactly
//! which merge shapes stay silent because of that, and #39 stays open for them.

use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error, ObjectId};
use tempfile::{tempdir, TempDir};

fn ref_of(result: &CasResult) -> String {
    match result {
        CasResult::Updated { name, .. } | CasResult::Noop { name, .. } => name.clone(),
        CasResult::Forked { fork, .. } => fork.clone(),
    }
}

/// A repository whose `main` holds `/a.txt`, `/d/one.txt` and `/d/sub/two.txt`.
struct Fixture {
    _dir: TempDir,
    forge: Forge,
    root: Cap,
    seeded: String,
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let integrator = forge.integrator_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge.write(&root, &ns, "/a.txt", b"hello", false).unwrap();
    forge
        .write(&root, &ns, "/d/one.txt", b"one", false)
        .unwrap();
    forge
        .write(&root, &ns, "/d/sub/two.txt", b"two", false)
        .unwrap();
    let seed = forge.checkin(&root, &ns, "/", "seed").unwrap();
    forge
        .merge(&integrator, "main", &ref_of(&seed), None)
        .unwrap();
    Fixture {
        _dir: dir,
        forge,
        root,
        seeded: "main".into(),
    }
}

impl Fixture {
    fn session(&self) -> String {
        self.forge.session_open(&self.root, &self.seeded).unwrap()
    }

    /// Every blob under `/`, as `path -> content`, resolved through the session.
    fn tree(&self, ns: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut pending = vec![String::from("/")];
        while let Some(at) = pending.pop() {
            for (name, kind, _, _) in self.forge.ls(&self.root, ns, &at).unwrap() {
                let path = if at == "/" {
                    format!("/{name}")
                } else {
                    format!("{at}/{name}")
                };
                if kind == "tree" {
                    pending.push(path);
                } else {
                    let data = self.forge.read(&self.root, ns, &path).unwrap();
                    out.push((path, String::from_utf8(data).unwrap()));
                }
            }
        }
        out.sort();
        out
    }
}

/// A blob move stages the destination and the tombstone together, and publishes
/// through the unchanged single-CAS checkin.
#[test]
fn i24_a_blob_move_publishes_as_one_contribution() {
    let f = fixture();
    let ns = f.session();
    let moved = f
        .forge
        .rename(&f.root, &ns, "/a.txt", "/b.txt", None)
        .unwrap();
    assert_eq!(moved.kind, "blob");
    assert_eq!(moved.entries, 1);

    let published = f.forge.checkin(&f.root, &ns, "/", "move").unwrap();
    let CasResult::Updated { oid, .. } = published else {
        panic!("a private checkin updates its own ref");
    };
    let (_, commit) = f.forge.peel_commit(&format!("oid:{oid}")).unwrap();
    assert!(
        commit.contrib.is_some(),
        "I10: a material checkin carries exactly one receipt"
    );

    let after = f
        .forge
        .session_open(&f.root, &format!("oid:{oid}"))
        .unwrap();
    assert_eq!(
        f.tree(&after),
        vec![
            ("/b.txt".to_string(), "hello".to_string()),
            ("/d/one.txt".to_string(), "one".to_string()),
            ("/d/sub/two.txt".to_string(), "two".to_string()),
        ]
    );
}

/// A directory move rewrites the tree rather than copying bytes: the moved
/// subtree keeps its ObjectId, which is only possible if the same tree object
/// is reachable at the new name (I2).
#[test]
fn i24_a_directory_move_reuses_the_subtree_object() {
    let f = fixture();
    let ns = f.session();
    let before = f
        .forge
        .ls(&f.root, &ns, "/")
        .unwrap()
        .into_iter()
        .find(|(n, ..)| n == "d")
        .expect("seeded directory");

    let moved = f.forge.rename(&f.root, &ns, "/d", "/e", None).unwrap();
    assert_eq!(moved.kind, "tree");
    assert_eq!(moved.entries, 2, "one row per moved file, not per entry");
    assert_eq!(
        moved.source.to_string(),
        before.2,
        "the move observes the subtree it is moving"
    );

    let CasResult::Updated { oid, .. } = f.forge.checkin(&f.root, &ns, "/", "move dir").unwrap()
    else {
        panic!("a private checkin updates its own ref");
    };
    let after = f
        .forge
        .session_open(&f.root, &format!("oid:{oid}"))
        .unwrap();
    let e = f
        .forge
        .ls(&f.root, &after, "/")
        .unwrap()
        .into_iter()
        .find(|(n, ..)| n == "e")
        .expect("moved directory");
    assert_eq!(e.1, "tree");
    assert_eq!(
        e.2, before.2,
        "a move is a tree rewrite; a copy would have produced the same oid only \
         by accident and a re-encode would not have"
    );
    assert_eq!(
        f.tree(&after),
        vec![
            ("/a.txt".to_string(), "hello".to_string()),
            ("/e/one.txt".to_string(), "one".to_string()),
            ("/e/sub/two.txt".to_string(), "two".to_string()),
        ]
    );
}

/// A move carries the session's own staged edits with it, because it moves what
/// the session can SEE, not what its pin holds.
#[test]
fn i24_a_move_carries_the_sessions_own_staged_edits() {
    let f = fixture();
    let ns = f.session();
    f.forge
        .write(&f.root, &ns, "/d/three.txt", b"three", false)
        .unwrap();
    f.forge.delete(&f.root, &ns, "/d/one.txt").unwrap();
    let moved = f.forge.rename(&f.root, &ns, "/d", "/e", None).unwrap();
    assert_eq!(
        moved.entries, 2,
        "two.txt and three.txt; one.txt was deleted"
    );

    let CasResult::Updated { oid, .. } = f.forge.checkin(&f.root, &ns, "/", "move dir").unwrap()
    else {
        panic!("a private checkin updates its own ref");
    };
    let after = f
        .forge
        .session_open(&f.root, &format!("oid:{oid}"))
        .unwrap();
    assert_eq!(
        f.tree(&after),
        vec![
            ("/a.txt".to_string(), "hello".to_string()),
            ("/e/sub/two.txt".to_string(), "two".to_string()),
            ("/e/three.txt".to_string(), "three".to_string()),
        ]
    );
}

/// I9: a move reads its source, so the read is recorded and a concurrent
/// publication of that path makes the move's own checkin stale rather than
/// silently moving bytes nobody looked at.
#[test]
fn i9_a_move_records_the_read_of_its_source() {
    let f = fixture();
    let integrator = f.forge.integrator_cap().unwrap();
    let mover = f.session();
    let other = f.session();

    f.forge
        .write(&f.root, &other, "/a.txt", b"changed", false)
        .unwrap();
    let published = f.forge.checkin(&f.root, &other, "/", "edit").unwrap();
    f.forge
        .merge(&integrator, "main", &ref_of(&published), None)
        .unwrap();

    f.forge
        .rename(&f.root, &mover, "/a.txt", "/b.txt", None)
        .unwrap();
    // The mover's own ref still CASes cleanly -- it is private -- so the
    // evidence that the read was recorded is the integration of the move into
    // the ref that moved under it.
    let moved = f.forge.checkin(&f.root, &mover, "/", "move").unwrap();
    let err = f
        .forge
        .merge(&integrator, "main", &ref_of(&moved), None)
        .expect_err("a move of a path another agent replaced must not be silent");
    assert!(
        matches!(err, Error::MergeConflict(_)),
        "expected a Conflict object, got {err:?}"
    );
}

/// `--expect-oid` is the caller's assumption about what it is moving, and a
/// wrong one is a stale observation (exit 4), not a move of something else.
#[test]
fn i24_expect_oid_refuses_a_source_the_caller_did_not_observe() {
    let f = fixture();
    let ns = f.session();
    let wrong = ObjectId([7u8; 32]);
    let err = f
        .forge
        .rename(&f.root, &ns, "/a.txt", "/b.txt", Some(wrong))
        .expect_err("a mismatched assumption must refuse");
    assert_eq!(
        err.exit_code(),
        4,
        "CLI_ABI maps a stale assumption to exit 4"
    );

    // Nothing was staged by the refusal.
    assert_eq!(
        f.tree(&ns),
        vec![
            ("/a.txt".to_string(), "hello".to_string()),
            ("/d/one.txt".to_string(), "one".to_string()),
            ("/d/sub/two.txt".to_string(), "two".to_string()),
        ]
    );

    let right = f
        .forge
        .ls(&f.root, &ns, "/")
        .unwrap()
        .into_iter()
        .find(|(n, ..)| n == "a.txt")
        .map(|(_, _, id, _)| ObjectId::from_hex(&id).unwrap())
        .unwrap();
    f.forge
        .rename(&f.root, &ns, "/a.txt", "/b.txt", Some(right))
        .expect("the observed oid is accepted");
}

/// Every refusal, and the exit code CLI_ABI.md gives it. A move that cannot be
/// made atomically is refused, never half-applied.
#[test]
fn i24_refusals_never_stage_a_partial_move() {
    let f = fixture();
    let ns = f.session();

    let before = f.tree(&ns);
    let cases: [(&str, &str, &str); 6] = [
        ("/absent.txt", "/x.txt", "not_found"),
        ("/", "/x", "invalid"),
        ("/a.txt", "/", "invalid"),
        ("/a.txt", "/a.txt", "invalid"),
        ("/d", "/d/inner", "invalid"),
        ("/d/sub", "/d", "invalid"),
    ];
    for (from, to, code) in cases {
        let Err(err) = f.forge.rename(&f.root, &ns, from, to, None) else {
            panic!("{from} -> {to} must refuse");
        };
        assert_eq!(err.code(), code, "{from} -> {to} classified as {err}");
        assert_eq!(
            err.exit_code(),
            1,
            "CLI_ABI puts every one of these on exit 1"
        );
        assert_eq!(
            f.tree(&ns),
            before,
            "{from} -> {to} refused but staged something"
        );
    }
}

/// A move never spans mounts: two mounts pin two refs (I19) and publish
/// separately, so there is no transaction that could carry both halves.
#[test]
fn i19_a_move_across_mounts_is_refused_not_split() {
    let f = fixture();
    let ns = f.session();
    f.forge.branch(&f.root, "main", "heads/side").unwrap();
    f.forge
        .mount(&f.root, &ns, "/side", "ref:heads/side", true)
        .unwrap();

    let err = f
        .forge
        .rename(&f.root, &ns, "/a.txt", "/side/moved.txt", None)
        .expect_err("a cross-mount move must refuse");
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string().contains("crosses mounts"),
        "the refusal must say why: {err}"
    );

    // Neither side was touched.
    assert!(f
        .forge
        .read(&f.root, &ns, "/a.txt")
        .is_ok_and(|d| d == b"hello"));
    assert!(
        f.forge.read(&f.root, &ns, "/side/moved.txt").is_err(),
        "the refused move must not have staged anything on the other mount"
    );
}

/// A read-only mount refuses a move for the same reason it refuses a write:
/// nothing could ever publish it (I20).
#[test]
fn i20_a_read_only_mount_refuses_a_move() {
    let f = fixture();
    let ns = f.session();
    f.forge
        .mount(&f.root, &ns, "/ro", "ref:main", false)
        .unwrap();
    let err = f
        .forge
        .rename(&f.root, &ns, "/ro/a.txt", "/ro/b.txt", None)
        .expect_err("a read-only mount cannot stage a move");
    assert_eq!(err.code(), "denied");
    assert_eq!(err.exit_code(), 1);
}

/// Kinds are not laundered by a move: what was a directory arrives as a
/// directory and what was a blob arrives as a blob (I16 names, I1 kinds).
#[test]
fn i24_a_move_preserves_entry_kind() {
    let f = fixture();
    let ns = f.session();
    f.forge.rename(&f.root, &ns, "/d", "/e", None).unwrap();
    f.forge
        .rename(&f.root, &ns, "/a.txt", "/f.txt", None)
        .unwrap();
    let CasResult::Updated { oid, .. } = f.forge.checkin(&f.root, &ns, "/", "move").unwrap() else {
        panic!("a private checkin updates its own ref");
    };
    let after = f
        .forge
        .session_open(&f.root, &format!("oid:{oid}"))
        .unwrap();
    let kinds: Vec<(String, String)> = f
        .forge
        .ls(&f.root, &after, "/")
        .unwrap()
        .into_iter()
        .map(|(n, k, ..)| (n, k))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("e".to_string(), "tree".to_string()),
            ("f.txt".to_string(), "blob".to_string()),
        ]
    );
}
