//! I5/I18/I13 (#343): the fork a losing checkin retargets a session at must be
//! a ref that session's own capability can reach.
//!
//! I5 forks a losing CAS and I18 retargets the session's mount at the result,
//! so unlike a merge fork this ref is one the loser is immediately expected to
//! act through. It used to be minted at `forks/<ref>/<agent>/<ulid>`, outside
//! the scope an agent capability is written with (`heads/agents/<id>/*`), so
//! every verb through the retargeted mount answered `Denied` -- reads included,
//! and including the session's own `/`. Nothing was lost, but the work I18
//! preserved sat at a name its own author could not open, and recovering it
//! needed a capability the agent did not hold.
//!
//! It is minted at `heads/agents/<agent>/forks/<ref>/<ulid>` now. That is the
//! option this closes on and the reason it is compatible with I13: no
//! capability is re-issued, re-signed, or widened. The loser's token is
//! byte-for-byte the one it already held, and the ref is simply created inside
//! the subtree that token already covered. `every_rw_mount_spec_is_refused_or_\
//! publishable` in `mount_protection.rs` covers the mount side of the same
//! property.
//!
//! The rejected alternative was to grant the session coverage of `forks/**` at
//! session open. Attenuation is monotone (I13) and a holder cannot widen its
//! own token, so that grant would have to be minted from the root secret on the
//! caller's behalf -- making `session open` an authority-amplification
//! primitive and exactly the ambient root I14 forbids.

use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

const AGENT: &str = "a1";

/// A capability scoped the way an agent's is: its own subtree, plus the shared
/// refs it was told to work on. Deliberately NOT `forks/*`.
fn agent_cap(f: &Forge, root: &Cap) -> Cap {
    f.grant(
        root,
        vec![
            "ops=read,write,branch".into(),
            format!("agent={AGENT}"),
            format!("ref=main,shared,heads/agents/{AGENT}/*"),
        ],
    )
    .unwrap()
}

fn seed(f: &Forge, root: &Cap, r: &str, path: &str, data: &[u8]) {
    let ns = f.session_open(root, r).unwrap();
    f.mount(root, &ns, "/", &format!("ref:{r}"), true).unwrap();
    f.write(root, &ns, path, data, false).unwrap();
    assert!(matches!(
        f.checkin(root, &ns, "/", "seed").unwrap(),
        CasResult::Updated { .. }
    ));
    f.abandon_session(root, &ns, false).unwrap();
}

/// I18/I13. A losing checkin on a shared ref forks, and the losing agent can
/// still read, write and publish through the mount that fork retargeted.
#[test]
fn a_losing_agent_can_reach_the_fork_its_own_checkin_produced() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();
    let agent = agent_cap(&f, &root);

    let ns = f.session_open(&agent, "main").unwrap();
    f.mount(&agent, &ns, "/w", "ref:shared", true).unwrap();
    f.write(&agent, &ns, "/w/mine.txt", b"mine", false).unwrap();

    // Someone else advances `shared` off this mount's pin.
    seed(&f, &root, "shared", "/theirs.txt", b"theirs");

    let outcome = f.checkin(&agent, &ns, "/w", "lose the race").unwrap();
    let CasResult::Forked { fork, .. } = &outcome else {
        panic!("expected a lost CAS to fork: {outcome:?}");
    };
    assert!(
        fork.starts_with(&format!("heads/agents/{AGENT}/forks/shared/")),
        "#343: the fork must land inside the losing agent's own scope: {fork}"
    );

    // THE POINT. The mount is retargeted at the fork; the loser can still use
    // it with the capability it already held.
    assert_eq!(
        f.read(&agent, &ns, "/w/mine.txt").unwrap(),
        b"mine",
        "I18 preserved the work; the agent must be able to read it back"
    );
    let listed: Vec<String> = f
        .ls(&agent, &ns, "/w")
        .expect("ls through the retargeted mount")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert!(listed.contains(&"mine.txt".to_string()), "{listed:?}");

    // And it can keep working on it and publish onto its own fork.
    f.write(&agent, &ns, "/w/more.txt", b"more", false)
        .expect("write through the retargeted mount");
    let published = f
        .checkin(&agent, &ns, "/w", "continue on the fork")
        .expect("checkin onto the fork the session was retargeted to");
    assert!(
        matches!(&published, CasResult::Updated { name, .. } if name == fork),
        "{published:?}"
    );

    // I18's terminal states are both available to the loser itself: retire the
    // session, then retire the fork. `abandon_fork` asks for write authority
    // over the fork's own name, which the agent now genuinely has.
    f.abandon_session(&agent, &ns, false).unwrap();
    f.abandon_fork(&agent, fork)
        .expect("the agent that forked can retire its own fork");
}

/// The session's own `/`, which is the case that made this worst: the root
/// mount is retargeted too, so an unreachable fork made the whole session
/// opaque to its owner.
#[test]
fn a_losing_agent_can_still_use_its_own_root_mount_after_a_fork() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "side").unwrap();
    seed(&f, &root, "side", "/s.txt", b"s");
    let agent = agent_cap(&f, &root);

    let ns = f.session_open(&agent, "main").unwrap();
    f.write(&agent, &ns, "/mine.txt", b"mine", false).unwrap();

    // An integrator advances the agent's own live head off its pin.
    let live = f
        .refs(&root)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .find(|n| n.starts_with(&format!("heads/agents/{AGENT}/")))
        .expect("session_open publishes a live head");
    f.merge(&root, &live, "side", None).unwrap();

    let outcome = f.checkin(&agent, &ns, "/", "lose on my own head").unwrap();
    let CasResult::Forked { fork, .. } = &outcome else {
        panic!("expected a fork: {outcome:?}");
    };
    assert_eq!(
        f.read(&agent, &ns, "/mine.txt").unwrap(),
        b"mine",
        "#343: the session's own / must stay readable after it is retargeted"
    );
    f.abandon_session(&agent, &ns, false).unwrap();
    f.abandon_fork(&agent, fork).unwrap();
}

/// I13, the other direction: the fork landing inside the agent's subtree must
/// not have widened what that capability reaches. It still cannot touch the
/// flat `forks/` tree, nor another agent's subtree.
#[test]
fn i13_the_fork_namespace_move_grants_no_reach_the_capability_lacked() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();
    let agent = agent_cap(&f, &root);

    // A merge fork, which still lives under the flat `forks/` tree because it
    // retargets no session and hands the loser nothing to act through.
    f.branch(&root, "main", "other").unwrap();
    seed(&f, &root, "other", "/o.txt", b"o");

    // Nothing under `forks/` is reachable by this capability.
    let ns = f.session_open(&agent, "main").unwrap();
    for name in [
        "forks/shared/a1/01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "heads/agents/a2/forks/shared/01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "heads/agents/a2/01ARZ3NDEKTSV4RRFFQ69G5FAV",
    ] {
        let err = f
            .mount(&agent, &ns, "/x", &format!("ref:{name}"), false)
            .expect_err("out of scope");
        assert!(
            matches!(err, Error::Denied(_) | Error::Cap(_)),
            "capability must still refuse {name}, got {err:?}"
        );
        let err = f.abandon_fork(&agent, name).expect_err("out of scope");
        assert!(
            matches!(err, Error::Denied(_) | Error::Cap(_)),
            "capability must still refuse to retire {name}, got {err:?}"
        );
    }
    f.abandon_session(&agent, &ns, false).unwrap();
}

/// I18. `abandon` still retires a fork, and still refuses everything that is
/// not one -- including a live session head, which now shares the `heads/`
/// prefix with every session fork.
#[test]
fn i18_abandon_retires_forks_and_nothing_else() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "shared").unwrap();

    let ns = f.session_open(&root, "main").unwrap();
    let live = f
        .refs(&root)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .find(|n| n.starts_with("heads/agents/"))
        .unwrap();

    for name in [live.as_str(), "main", "shared"] {
        let err = f.abandon_fork(&root, name).expect_err("not a fork");
        assert!(
            matches!(err, Error::Invalid(_) | Error::Denied(_)),
            "abandon must refuse {name}, got {err:?}"
        );
    }
    f.abandon_session(&root, &ns, false).unwrap();
}
