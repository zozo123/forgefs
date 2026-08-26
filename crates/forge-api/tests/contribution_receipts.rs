//! I25: a receipt is a claim that can be checked, and it is checked (#71).
//!
//! `Contribution` (`0x06`) has been the contribution receipt since #10 -- base,
//! tree, parents, the observed blob frontier, the written paths, the agent.
//! What was missing is the half #71 calls invariant 4: *a receipt cannot claim
//! a result commit or observation OID that is absent or corrupt*. Nothing
//! checked. `forge show` renders a Contribution's fields without touching a
//! single object it names, so a receipt for a commit whose tree had been
//! collected, truncated or replaced printed exactly like a good one.
//!
//! Every refusal below is produced by removing or crafting a real object file
//! under `.forge/objects/`, and each is paired with the `show` output for the
//! same object, which still renders. That pairing is what makes these tests
//! statements about `receipt` rather than about the fixture.

use forge_api::Forge;
use forge_cap::Cap;
use forge_core::object::hash_bytes;
use forge_core::{Commit, Contribution};
use forge_types::{CasResult, ObjectId};
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

fn ref_of(result: &CasResult) -> String {
    match result {
        CasResult::Updated { name, .. } | CasResult::Noop { name, .. } => name.clone(),
        CasResult::Forked { fork, .. } => fork.clone(),
    }
}

struct Fixture {
    dir: TempDir,
    forge: Forge,
    root: Cap,
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    Fixture { dir, forge, root }
}

impl Fixture {
    /// Publish one checkin that reads `/seed.txt` and writes two paths.
    fn contribute(&self) -> String {
        let ns = self.forge.session_open(&self.root, "main").unwrap();
        self.forge
            .write(&self.root, &ns, "/seed.txt", b"seed", false)
            .unwrap();
        let seeded = self.forge.checkin(&self.root, &ns, "/", "seed").unwrap();

        let work = self
            .forge
            .session_open(&self.root, &ref_of(&seeded))
            .unwrap();
        // I9: a recorded read is what puts an entry in the receipt's frontier.
        self.forge.read(&self.root, &work, "/seed.txt").unwrap();
        self.forge
            .write(&self.root, &work, "/a.txt", b"alpha", false)
            .unwrap();
        self.forge
            .write(&self.root, &work, "/b.txt", b"beta", false)
            .unwrap();
        ref_of(&self.forge.checkin(&self.root, &work, "/", "work").unwrap())
    }

    fn object_path(&self, oid: ObjectId) -> PathBuf {
        let (a, b) = oid.shard_dirs();
        self.dir
            .path()
            .join(".forge/objects")
            .join(a)
            .join(b)
            .join(oid.hex())
    }

    /// Write raw bytes into the object store by hand, the way a corruption
    /// test has to: the API refuses to publish an object that does not check
    /// out, which is exactly the property under test.
    fn plant(&self, bytes: &[u8]) -> ObjectId {
        let oid = hash_bytes(bytes);
        let path = self.object_path(oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        oid
    }

    fn show(&self, oid: ObjectId) -> String {
        self.forge
            .show(&self.root, &format!("oid:{oid}"))
            .expect("show renders without checking anything")
    }
}

fn remove(path: &Path) {
    std::fs::remove_file(path).expect("object file exists before the test removes it");
}

/// The receipt a checkin published, reached through the ref it advanced.
#[test]
fn i25_a_receipt_reports_the_frontier_and_the_commit_that_published_it() {
    let f = fixture();
    let published = f.contribute();

    let receipt = f.forge.receipt(&f.root, &published).unwrap();
    let (commit_oid, commit) = f.forge.peel_commit(&published).unwrap();

    assert_eq!(
        receipt.result,
        Some(commit_oid),
        "a receipt reached through a commit names that commit"
    );
    assert_eq!(receipt.receipt, commit.contrib.unwrap());
    assert_eq!(receipt.tree, commit.tree);
    assert_eq!(receipt.parents, commit.parents);
    assert_eq!(receipt.agent, commit.agent);

    // I9: the observed frontier, and only blob observations (VERSION 1 `reads`
    // cannot express a directory or an absence).
    assert_eq!(
        receipt
            .reads
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>(),
        vec!["/seed.txt"]
    );
    assert_eq!(receipt.writes, vec!["/a.txt".to_string(), "/b.txt".into()]);

    // Reached by the receipt oid alone, there is no commit to name.
    let direct = f
        .forge
        .receipt(&f.root, &format!("oid:{}", receipt.receipt))
        .unwrap();
    assert_eq!(direct.result, None);
    assert_eq!(direct.writes, receipt.writes);

    let rendered = receipt.render();
    assert!(
        rendered.contains(&format!("result {commit_oid}")),
        "{rendered}"
    );
    assert!(rendered.contains("verified 5 edges"), "{rendered}");
}

/// #71 invariant 4, the absent half. Removing ONE object the receipt names is
/// enough, and `show` of the same receipt still renders every field of it --
/// which is what this test is for.
#[test]
fn i25_a_receipt_naming_an_absent_object_is_refused_where_show_still_renders_it() {
    let f = fixture();
    let published = f.contribute();
    let receipt = f.forge.receipt(&f.root, &published).unwrap();

    remove(&f.object_path(receipt.tree));

    // The unchecked path is unaffected: this is the behaviour that made the
    // defect invisible, kept here so the refusal below cannot be mistaken for
    // a property of the fixture.
    let shown = f.show(receipt.receipt);
    assert!(
        shown.contains(&format!("tree {}", receipt.tree)),
        "show still prints the missing tree: {shown}"
    );

    let err = f
        .forge
        .receipt(&f.root, &published)
        .expect_err("a receipt naming an absent tree must refuse");
    assert_eq!(err.exit_code(), 2, "CLI_ABI maps a corrupt graph to exit 2");
    assert!(
        err.to_string().contains(&receipt.tree.hex()),
        "the refusal must name the edge that failed: {err}"
    );
}

/// The same, for an observation rather than the tree: a receipt may not claim
/// it read bytes that are not there.
#[test]
fn i25_a_receipt_naming_an_absent_observation_is_refused() {
    let f = fixture();
    let published = f.contribute();
    let receipt = f.forge.receipt(&f.root, &published).unwrap();
    let (_, observed) = receipt.reads[0].clone();

    remove(&f.object_path(observed));

    let err = f
        .forge
        .receipt(&f.root, &published)
        .expect_err("a receipt naming an absent observation must refuse");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains(&observed.hex()), "{err}");
}

/// #71 invariant 4, the wrongly-typed half. A receipt whose `tree` is really a
/// blob is refused by TYPE, not merely by presence: every byte is there and
/// rehashes, and it is still not a receipt.
#[test]
fn i25_a_receipt_whose_edge_has_the_wrong_type_is_refused() {
    let f = fixture();
    let published = f.contribute();
    let honest = f.forge.receipt(&f.root, &published).unwrap();
    let (_, blob) = honest.reads[0].clone();

    let forged = Contribution {
        base: honest.base,
        // A real, present, correctly hashed object -- of the wrong type.
        tree: blob,
        parents: honest.parents.clone(),
        reads: vec![],
        writes: vec!["/a.txt".into()],
        agent: honest.agent.clone(),
        ts: honest.ts,
    };
    let planted = f.plant(&forged.encode().unwrap());

    // Again: `show` is happy to render it.
    let shown = f.show(planted);
    assert!(shown.contains(&format!("tree {blob}")), "{shown}");

    let err = f
        .forge
        .receipt(&f.root, &format!("oid:{planted}"))
        .expect_err("a receipt whose tree is a blob must refuse");
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.to_string().contains("but it is blob"),
        "the refusal must say what it found: {err}"
    );
}

/// A receipt reached through a commit is a claim ABOUT that commit, so the
/// commit has to agree with it. Every object here is present and correctly
/// typed; the defect is only that the two disagree.
#[test]
fn i25_a_commit_that_disagrees_with_its_receipt_is_refused() {
    let f = fixture();
    let published = f.contribute();
    let honest = f.forge.receipt(&f.root, &published).unwrap();

    // A second, unrelated tree that is genuinely present.
    let other_ns = f.forge.session_open(&f.root, "main").unwrap();
    f.forge
        .write(&f.root, &other_ns, "/elsewhere.txt", b"other", false)
        .unwrap();
    let other = f.forge.checkin(&f.root, &other_ns, "/", "other").unwrap();
    let (_, other_commit) = f.forge.peel_commit(&ref_of(&other)).unwrap();
    assert_ne!(other_commit.tree, honest.tree);

    let forged = Contribution {
        base: honest.base,
        tree: other_commit.tree,
        parents: honest.parents.clone(),
        reads: vec![],
        writes: vec!["/a.txt".into()],
        agent: honest.agent.clone(),
        ts: honest.ts,
    };
    let receipt_oid = f.plant(&forged.encode().unwrap());
    // On its own the forged receipt verifies: every edge is present and typed.
    f.forge
        .receipt(&f.root, &format!("oid:{receipt_oid}"))
        .expect("its own edges are sound");

    let commit = Commit {
        tree: honest.tree,
        parents: honest.parents.clone(),
        agent: honest.agent.clone(),
        msg: "claims a receipt that describes a different tree".into(),
        ts: honest.ts,
        landmark: false,
        contrib: Some(receipt_oid),
    };
    let commit_oid = f.plant(&commit.encode());

    let err = f
        .forge
        .receipt(&f.root, &format!("oid:{commit_oid}"))
        .expect_err("a commit may not name a receipt describing other work");
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.to_string().contains("disagrees with the receipt"),
        "{err}"
    );
}

/// I10 makes a missing receipt legitimate rather than corrupt, so a commit
/// that has none is exit 1 and not exit 2, and the diagnostic says which.
#[test]
fn i10_a_commit_without_a_receipt_is_absence_and_not_corruption() {
    let f = fixture();
    let err = f
        .forge
        .receipt(&f.root, "main")
        .expect_err("the bootstrap commit carries no receipt");
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("no contribution receipt"), "{err}");

    // A merge commit is the other legitimate case.
    let integrator = f.forge.integrator_cap().unwrap();
    let published = f.contribute();
    f.forge
        .merge(&integrator, "main", &published, None)
        .unwrap();
    let err = f
        .forge
        .receipt(&f.root, "main")
        .expect_err("a merge commit carries no receipt of its own");
    assert_eq!(err.exit_code(), 1);
}

/// The object the CALLER named and the objects a RECEIPT names get different
/// answers on purpose: a caller naming something this repository does not hold
/// made a not-found request (exit 1); a receipt naming something that is not
/// held is a corrupt graph (exit 2). Collapsing the two would report a typo as
/// a damaged repository, and a damaged repository as a typo.
#[test]
fn absence_the_caller_named_is_input_and_absence_a_receipt_named_is_corruption() {
    let f = fixture();
    let published = f.contribute();
    let receipt = f.forge.receipt(&f.root, &published).unwrap();

    let absent = ObjectId([0u8; 32]);
    assert_eq!(
        f.forge
            .receipt(&f.root, &format!("oid:{absent}"))
            .expect_err("this repository does not hold that object")
            .exit_code(),
        1,
    );

    remove(&f.object_path(receipt.base));
    assert_eq!(
        f.forge
            .receipt(&f.root, &published)
            .expect_err("the receipt names a base that is gone")
            .exit_code(),
        2,
    );
}

/// I13/I14: reading a receipt is reading a ref. A cap that cannot read the ref
/// cannot read what it published, and a ref-scoped cap cannot launder that by
/// naming the receipt's raw object id.
#[test]
fn i13_a_receipt_is_readable_only_through_authority_over_what_names_it() {
    let f = fixture();
    let published = f.contribute();
    let receipt = f.forge.receipt(&f.root, &published).unwrap();

    let scoped = f
        .forge
        .grant(
            &f.root,
            vec!["ops=read".into(), "agent=alice".into(), "ref=main".into()],
        )
        .unwrap();
    assert_eq!(
        f.forge
            .receipt(&scoped, &published)
            .expect_err("out of scope")
            .exit_code(),
        1
    );
    assert_eq!(
        f.forge
            .receipt(&scoped, &format!("oid:{}", receipt.receipt))
            .expect_err("a ref-scoped cap may not address raw object ids")
            .exit_code(),
        1
    );
}

/// Verification rereads durable bytes rather than the hot caches (I15), so an
/// object removed underneath a live `Forge` is caught by the process that
/// removed it and not only after a cold reopen.
#[test]
fn i15_receipt_verification_does_not_answer_out_of_the_cache() {
    let f = fixture();
    let published = f.contribute();
    let receipt = f.forge.receipt(&f.root, &published).unwrap();

    // Warm every cache this Forge keeps for the receipt's own edges.
    for _ in 0..3 {
        f.forge.receipt(&f.root, &published).unwrap();
    }
    remove(&f.object_path(receipt.tree));

    // Same live Forge, no reopen.
    let err = f
        .forge
        .receipt(&f.root, &published)
        .expect_err("a cached copy is not evidence the object is there");
    assert_eq!(err.exit_code(), 2);
}
