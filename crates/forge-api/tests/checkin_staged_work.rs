//! #326 / I19: a checkin publishes everything the session has staged, or
//! refuses and names what it would leave behind. It never reports success over
//! work it did not fold.
//!
//! Written as the general property, not as the single reported case: every
//! placement of staged work across a session's read-write mounts, crossed with
//! every mount checkin can be asked to publish. The oracle is
//! `abandon_session`, the verb that already counts overlay rows across the
//! whole namespace. Whatever checkin answers, the two must agree on the one
//! question they both ask -- does this session still hold staged work.

use forge_api::Forge;
use forge_types::{CasResult, Error};
use tempfile::tempdir;

const MOUNTS: [&str; 3] = ["/", "/s", "/t"];

fn staged_path(mount: &str) -> String {
    if mount == "/" {
        "/staged.txt".to_string()
    } else {
        format!("{mount}/staged.txt")
    }
}

/// Stage work under each mount in `staged`, check in `checkin_mount`, and
/// assert the property for that placement.
fn check_placement(staged: &[&str], checkin_mount: &str) {
    let label = format!("staged={staged:?} checkin={checkin_mount}");
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "shared-s").unwrap();
    forge.branch(&root, "main", "shared-t").unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    // `/` is the session's own live ref, created by session_open.
    forge.mount(&root, &ns, "/s", "ref:shared-s", true).unwrap();
    forge.mount(&root, &ns, "/t", "ref:shared-t", true).unwrap();
    for mount in staged {
        forge
            .write(&root, &ns, &staged_path(mount), b"work", false)
            .unwrap();
    }

    let stranded: Vec<&&str> = staged.iter().filter(|m| **m != checkin_mount).collect();
    let outcome = forge.checkin(&root, &ns, checkin_mount, "m");

    match &outcome {
        Ok(CasResult::Noop { .. }) => {
            // The defect: `noop` with exit 0 is indistinguishable from "there
            // was nothing to do", so it may only be said when that is true.
            assert!(
                staged.is_empty(),
                "{label}: checkin reported a no-op success while the session held staged work"
            );
        }
        Ok(published) => {
            assert_eq!(
                staged.to_vec(),
                vec![checkin_mount],
                "{label}: checkin published {published:?} but the staged set was not exactly the \
                 mount it folded"
            );
            assert_eq!(
                forge.read(&root, &ns, &staged_path(checkin_mount)).unwrap(),
                b"work",
                "{label}: the published work must be readable back through its mount"
            );
        }
        Err(Error::Invalid(msg)) => {
            assert!(
                !stranded.is_empty(),
                "{label}: checkin refused with nothing staged outside its mount: {msg}"
            );
            for mount in &stranded {
                assert!(
                    msg.contains(&format!("{mount} (")),
                    "{label}: the refusal must name the mount {mount} that holds work: {msg}"
                );
            }
        }
        Err(other) => panic!("{label}: unexpected checkin failure {other:?}"),
    }

    // The property itself, stated once: checkin and abandon never disagree
    // about whether the session still holds staged work. A checkin that
    // succeeded published everything staged, so the session is retirable
    // without the discard flag; one that failed left the work where abandon
    // can still see it.
    let abandon = forge.abandon_session(&root, &ns, false);
    assert_eq!(
        outcome.is_ok(),
        abandon.is_ok(),
        "{label}: checkin said {outcome:?} but abandon said {abandon:?}"
    );
}

#[test]
fn checkin_never_reports_a_noop_success_over_staged_work() {
    // Every subset of the three read-write mounts, crossed with every mount
    // checkin can be asked to publish.
    for bits in 0..(1u8 << MOUNTS.len()) {
        let staged: Vec<&str> = MOUNTS
            .iter()
            .enumerate()
            .filter(|(i, _)| bits & (1 << i) != 0)
            .map(|(_, m)| *m)
            .collect();
        for checkin_mount in MOUNTS {
            check_placement(&staged, checkin_mount);
        }
    }
}

#[test]
fn checkin_of_the_default_mount_refuses_work_staged_through_another_rw_mount() {
    // The exact shape reported in #326: mount a shared ref read-write at a
    // path other than `/`, write through it, then run the checkin an agent
    // actually runs. It used to answer `noop` and exit 0 with the entry in no
    // ref at all, while `abandon` refused the same session as holding work.
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "shared").unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge.mount(&root, &ns, "/s", "ref:shared", true).unwrap();
    forge.write(&root, &ns, "/s/new.txt", b"hi", false).unwrap();

    let err = forge
        .checkin(&root, &ns, "/", "a")
        .expect_err("checkin must not report success over work it never folded");
    let msg = format!("{err}");
    assert!(
        matches!(err, Error::Invalid(_)),
        "the refusal is an input error (CLI_ABI exit 1), got {err:?}"
    );
    assert!(
        msg.contains("/s (1 staged entry)"),
        "the diagnostic must name the mount holding the work: {msg}"
    );

    // And the way out is not to discard: naming the mount publishes it.
    assert!(matches!(
        forge.checkin(&root, &ns, "/s", "a").unwrap(),
        CasResult::Updated { .. } | CasResult::Forked { .. }
    ));
    assert_eq!(forge.read(&root, &ns, "/s/new.txt").unwrap(), b"hi");
    forge
        .abandon_session(&root, &ns, false)
        .expect("a session whose work is published retires without discarding anything");
}
