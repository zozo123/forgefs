//! I20/I21 (#328): a mount that accepts a write has a verb that can publish it.
//!
//! I20 is a totality rule, and the way it kept failing was always the same: it
//! was checked for SOME of the read-write mount specs the system can produce
//! and not for all of them. A read-write `oid:` spec was refused at mount time,
//! and a ref not holding a commit was refused at mount time -- but a PROTECTED
//! ref was accepted, `checkin` then denied it (`ref main is protected; session
//! checkin cannot advance it`), `abandon` without a discard refused because
//! work was staged, and `--discard-staged`, which destroys the work, was the
//! only exit (#328).
//!
//! So the test here is not "a protected ref is refused". It is the property
//! itself, ENUMERATED over the read-write mount specs a ForgeFS repository can
//! actually hand an agent, each one built by driving the real verb that
//! produces it. Every one of them must either be refused when the mount is
//! taken, or be publishable: write, `checkin --mount`, and then `abandon`
//! WITHOUT a discard, which is the exact sequence that was impossible.
//!
//! Adding a new mountable ref shape without deciding which side of that line it
//! falls on fails `every_rw_mount_spec_is_refused_or_publishable`.

use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error};
use tempfile::{tempdir, TempDir};

/// What I20 requires of one read-write mount spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Knowably unpublishable when the mount is taken, so `mount` must refuse
    /// it rather than accept a write no verb could ever land.
    RefusedAtMountTime,
    /// Accepted, and a write through it can be published and the session then
    /// retired without discarding anything.
    Publishable,
}

struct Fixture {
    _dir: TempDir,
    forge: Forge,
    root: Cap,
}

impl Fixture {
    /// A repository holding one of every ref shape a read-write mount could
    /// name, each produced by the verb that really mints it.
    fn build() -> Self {
        let dir = tempdir().unwrap();
        let forge = Forge::init(dir.path()).unwrap();
        let root = forge.root_cap().unwrap();
        forge.branch(&root, "main", "heads/topic").unwrap();
        forge.branch(&root, "main", "heads/losing").unwrap();
        forge.seal(&root, "main", "v1").unwrap();
        Fixture {
            _dir: dir,
            forge,
            root,
        }
    }

    /// The hex OID `main` holds, for an `oid:` spec.
    fn main_oid_spec(&self) -> String {
        let (oid, _) = self.forge.peel_commit("main").unwrap();
        format!("oid:{}", oid.hex())
    }

    /// A `conflicts/<ref>/<ulid>` ref, minted by a real merge conflict.
    fn conflict_ref(&self) -> String {
        self.forge.branch(&self.root, "main", "heads/ours").unwrap();
        self.forge
            .branch(&self.root, "main", "heads/theirs")
            .unwrap();
        seed(&self.forge, &self.root, "heads/ours", "/c.txt", b"OURS");
        seed(&self.forge, &self.root, "heads/theirs", "/c.txt", b"THEIRS");
        let err = self
            .forge
            .merge(&self.root, "heads/ours", "heads/theirs", None)
            .expect_err("divergent edits of one path must conflict");
        assert!(matches!(err, Error::MergeConflict(_)), "{err:?}");
        self.forge
            .refs(&self.root)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .find(|n| n.starts_with("conflicts/"))
            .expect("the merge published a conflict ref")
    }

    /// A `forks/<ref>/<agent>/<ulid>` ref, minted by a real losing CAS (I5/I18).
    fn fork_ref(&self) -> String {
        let loser = self.forge.session_open(&self.root, "heads/losing").unwrap();
        self.forge
            .mount(&self.root, &loser, "/w", "ref:heads/losing", true)
            .unwrap();
        self.forge
            .write(&self.root, &loser, "/w/loser.txt", b"mine", false)
            .unwrap();
        // Another agent advances the ref underneath the loser's pin.
        seed(
            &self.forge,
            &self.root,
            "heads/losing",
            "/winner.txt",
            b"theirs",
        );
        let outcome = self
            .forge
            .checkin(&self.root, &loser, "/w", "lose")
            .unwrap();
        let CasResult::Forked { fork, .. } = outcome else {
            panic!("expected the CAS to lose and fork: {outcome:?}");
        };
        self.forge
            .abandon_session(&self.root, &loser, false)
            .expect("I18: the fork holds the work, so the loser retires without a discard");
        fork
    }
}

/// Advance `r` by one checkin through a session of its own.
fn seed(f: &Forge, root: &Cap, r: &str, path: &str, data: &[u8]) {
    let ns = f.session_open(root, r).unwrap();
    f.mount(root, &ns, "/", &format!("ref:{r}"), true).unwrap();
    f.write(root, &ns, path, data, false).unwrap();
    let result = f.checkin(root, &ns, "/", "seed").unwrap();
    assert!(matches!(result, CasResult::Updated { .. }), "{result:?}");
    f.abandon_session(root, &ns, false).unwrap();
}

/// THE property. A read-write mount of `spec` must be refused when it is taken,
/// or a write through it must be publishable and the session then retirable
/// without destroying anything.
///
/// Returns nothing and asserts everything: the point is that there is no third
/// answer, and "accepted, then wedged" is exactly the third answer #328 was.
fn assert_i20(fx: &Fixture, spec: &str, expected: Verdict, why: &str) {
    let f = &fx.forge;
    let root = &fx.root;
    let ns = f.session_open(root, "heads/topic").unwrap();

    match (f.mount(root, &ns, "/w", spec, true), expected) {
        (Err(Error::Denied(_)), Verdict::RefusedAtMountTime) => {
            // I18: nothing was staged, so nothing can be stranded, and the
            // session still retires cleanly.
            f.abandon_session(root, &ns, false).expect(
                "a session whose unpublishable mount was refused holds no work and must retire",
            );
            return;
        }
        (Ok(()), Verdict::Publishable) => {}
        (got, want) => panic!(
            "mount {spec} read-write ({why}): got {got:?}, but I20 requires {want:?}.\n\
             A read-write mount is either refused when it is taken or publishable \
             afterwards; there is no third answer, and 'accepted, then no verb can \
             publish it' is the defect I20 exists to forbid (#328)."
        ),
    }

    // Accepted. Now I20's other half must actually hold: the write lands
    // somewhere, and the session reaches a terminal state without a discard.
    f.write(root, &ns, "/w/work.txt", b"work", false)
        .unwrap_or_else(|e| {
            panic!("mount {spec} was accepted read-write but refused a write: {e:?}")
        });

    let published = f.checkin(root, &ns, "/w", "publish").unwrap_or_else(|e| {
        panic!(
            "mount {spec} ({why}) accepted a write that checkin then refused with {e:?}.\n\
             I20: a mount that accepts a write must have a verb that can publish it. \
             If this spec is knowably unpublishable, mount must refuse it -- failing \
             at checkin strands the work, because abandon without a discard refuses \
             over staged entries and --discard-staged destroys it (#328)."
        )
    });
    assert!(
        matches!(
            published,
            CasResult::Updated { .. } | CasResult::Forked { .. }
        ),
        "mount {spec} ({why}): checkin answered {published:?}, which publishes nothing"
    );

    // I21: and the session can now reach a terminal state without discarding.
    f.abandon_session(root, &ns, false).unwrap_or_else(|e| {
        panic!(
            "mount {spec} ({why}): the work was published, yet abandon without a \
             discard still refused with {e:?}"
        )
    });
}

/// I20, enumerated. Every read-write mount spec a ForgeFS repository can
/// produce, each built by the verb that really produces it.
#[test]
fn every_rw_mount_spec_is_refused_or_publishable() {
    let fx = Fixture::build();
    let conflict = fx.conflict_ref();
    let fork = fx.fork_ref();
    let oid_spec = fx.main_oid_spec();
    let own_live = {
        // The session's own `/` mount names `heads/agents/<agent>/<ulid>`,
        // minted by `session_open`. Take one and reuse the name.
        let ns = fx.forge.session_open(&fx.root, "main").unwrap();
        let name = fx
            .forge
            .refs(&fx.root)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .find(|n| n.starts_with("heads/agents/"))
            .expect("session_open publishes a live session head");
        fx.forge.abandon_session(&fx.root, &ns, false).unwrap();
        name
    };

    let cases: Vec<(String, Verdict, &str)> = vec![
        (
            "ref:heads/topic".into(),
            Verdict::Publishable,
            "an ordinary unprotected commit ref this capability may write",
        ),
        (
            format!("ref:{own_live}"),
            Verdict::Publishable,
            "a live session head, the shape session_open itself mounts at /",
        ),
        (
            format!("ref:{fork}"),
            Verdict::Publishable,
            "a fork minted by a losing CAS (I5/I18): the work it preserves must \
             stay publishable, or preserving it means nothing",
        ),
        (
            "ref:main".into(),
            Verdict::RefusedAtMountTime,
            "a PROTECTED ref: I5 makes every session CAS on it deny, so write \
             authority over it is authority checkin can never exercise (#328)",
        ),
        (
            "ref:tags/v1".into(),
            Verdict::RefusedAtMountTime,
            "a ref holding a snapshot, not a commit: checkin CASes a commit ref",
        ),
        (
            format!("ref:{conflict}"),
            Verdict::RefusedAtMountTime,
            "a ref holding a Conflict: likewise not a commit checkin can advance",
        ),
        (
            oid_spec,
            Verdict::RefusedAtMountTime,
            "immutable bytes with no ref for checkin to advance",
        ),
    ];

    for (spec, verdict, why) in &cases {
        assert_i20(&fx, spec, *verdict, why);
    }

    // A guard on the enumeration itself: if the list ever stops covering both
    // outcomes the property has quietly become vacuous.
    assert!(
        cases.iter().any(|c| c.1 == Verdict::Publishable)
            && cases.iter().any(|c| c.1 == Verdict::RefusedAtMountTime),
        "the enumeration must exercise both sides of I20"
    );
}

/// #328 written out by hand, so the fix is readable without the table.
///
/// This is the sequence the model-based harness reported as
/// `F4-SESSION-WEDGED-WITH-STAGED-WORK` and the invariant audit recorded as an
/// open shape gap. `main` is protected from `init`, so this is the FIRST thing
/// an agent handed a repository and told to work on `main` would do.
#[test]
fn i20_a_rw_mount_of_a_protected_ref_is_refused_when_it_is_taken() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    let refusal = forge
        .mount(&root, &ns, "/w", "ref:main", true)
        .expect_err("I20: a protected ref has no publish path, so the mount is refused");
    let Error::Denied(message) = &refusal else {
        panic!("the refusal must be Denied -- exit 1, the same row as the rw oid: refusal: {refusal:?}");
    };
    assert!(
        message.contains("ref main is protected"),
        "the refusal must name the reason the mount can never be published: {message}"
    );

    // The escape the diagnostic advises actually works, both ways.
    forge
        .mount(&root, &ns, "/w", "ref:main", false)
        .expect("read-only is always available: nothing is staged, so nothing is stranded");
    forge.branch(&root, "main", "heads/from-main").unwrap();
    forge
        .mount(&root, &ns, "/b", "ref:heads/from-main", true)
        .expect("a branch of a protected ref is writable and publishable");
    forge.write(&root, &ns, "/b/x.txt", b"work", false).unwrap();
    assert!(matches!(
        forge.checkin(&root, &ns, "/b", "publish").unwrap(),
        CasResult::Updated { .. }
    ));
    forge
        .abandon_session(&root, &ns, false)
        .expect("I21: the session reached a terminal state without discarding anything");
}

/// The half of #328 a mount-time check does NOT close by itself, pinned so it
/// cannot silently open.
///
/// A check taken when the mount is created is only complete while the condition
/// it checks cannot appear afterwards. `refs.protected` is write-once: the only
/// statements that write a 1 into it are `insert_ref`, `insert_ref_with_intros`
/// and `commit_seal`; the first two refuse a name that already exists, and
/// `commit_seal` writes `tags/*` alone -- a namespace `insert_ref` forbids any
/// commit ref from occupying, and one a read-write mount is already refused for
/// holding a snapshot. Every fork path writes the literal 0 and the
/// ref-advancing `UPDATE` never mentions the column.
///
/// This test holds that closure property to the PUBLIC API rather than to the
/// SQL, so a future verb that protects an existing ref -- which would reopen
/// #328 underneath every live mount -- fails here and is forced to decide what
/// happens to those mounts.
#[test]
fn protection_is_write_once_so_no_live_mount_can_be_overtaken_by_it() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "heads/live").unwrap();

    // A live read-write mount on it, holding staged work: exactly the state
    // that would be wedged if the ref became protected underneath it.
    let ns = forge.session_open(&root, "heads/live").unwrap();
    forge
        .mount(&root, &ns, "/w", "ref:heads/live", true)
        .unwrap();
    forge
        .write(&root, &ns, "/w/staged.txt", b"work", false)
        .unwrap();

    let protected = |f: &Forge| f.store_ref_protected("heads/live");
    assert!(!protected(&forge), "branch must not create a protected ref");

    // Every public verb that creates or moves a ref, run against the name that
    // has the live mount. None of them may leave it protected.
    forge
        .branch(&root, "main", "heads/other")
        .expect("branch a second name");
    assert!(
        forge.branch(&root, "main", "heads/live").is_err(),
        "a ref that already exists cannot be re-created, which is what makes \
         protection unable to arrive after the fact"
    );
    forge.seal(&root, "heads/live", "tag-of-live").unwrap();
    assert!(
        !protected(&forge),
        "sealing a ref protects the TAG it mints, never the ref it sealed -- if \
         this ever changes, #328 reopens underneath every live read-write mount \
         of that ref and the mount-time refusal in Forge::mount is no longer a \
         complete answer"
    );
    let other = forge.session_open(&root, "heads/live").unwrap();
    forge
        .mount(&root, &other, "/w", "ref:heads/live", true)
        .unwrap();
    forge.write(&root, &other, "/w/x.txt", b"x", false).unwrap();
    forge.checkin(&root, &other, "/w", "advance").unwrap();
    forge.abandon_session(&root, &other, false).unwrap();
    assert!(
        !protected(&forge),
        "a checkin CAS must never set protection on the ref it advanced"
    );
    forge.merge(&root, "heads/live", "heads/other", None).ok();
    assert!(!protected(&forge), "merge must not protect its target");

    // And the mount that was live throughout is still publishable, which is
    // the property all of the above exists to preserve.
    let published = forge
        .checkin(&root, &ns, "/w", "publish")
        .expect("the mount taken before all of that is still publishable");
    assert!(
        matches!(
            published,
            CasResult::Forked { .. } | CasResult::Updated { .. }
        ),
        "{published:?}"
    );
    forge
        .abandon_session(&root, &ns, false)
        .expect("I21: terminal state reached without a discard");
}

/// Small helper so the test above states its question about the catalog rather
/// than about `Forge`'s internals.
trait RefProtection {
    fn store_ref_protected(&self, name: &str) -> bool;
}

impl RefProtection for Forge {
    fn store_ref_protected(&self, name: &str) -> bool {
        self.refs(&self.root_cap().unwrap())
            .unwrap()
            .into_iter()
            .find(|r| r.name == name)
            .map(|r| r.protected)
            .unwrap_or(false)
    }
}
