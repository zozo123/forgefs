use forge_api::Forge;
use forge_types::CasResult;
use tempfile::tempdir;

#[test]
fn rw_mount_reads_pinned_base_then_lost_race_forks() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    f.branch(&root, "main", "shared").unwrap();

    // Seed shared with a stable value.
    let seed = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &seed, "/", "ref:shared", true).unwrap();
    f.write(&root, &seed, "/a.txt", b"v0", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &seed, "/", "seed").unwrap(),
        CasResult::Updated { .. }
    ));

    // A pins shared@v0.
    let a = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &a, "/", "ref:shared", true).unwrap();

    // B advances the mutable ref to v1 after A opened.
    let b = f.session_open(&root, "shared").unwrap();
    f.mount(&root, &b, "/", "ref:shared", true).unwrap();
    f.write(&root, &b, "/a.txt", b"v1", false).unwrap();
    assert!(matches!(
        f.checkin(&root, &b, "/", "advance").unwrap(),
        CasResult::Updated { .. }
    ));

    // I8: A's workspace reads must come from its pinned base, never the live ref.
    assert_eq!(f.read(&root, &a, "/a.txt").unwrap(), b"v0");

    // A's disjoint staged work is still valid. The shared-ref race is resolved by
    // the normal CAS loser path: preserve A's work under an explicit fork.
    f.write(&root, &a, "/b.txt", b"mine", false).unwrap();
    let result = f.checkin(&root, &a, "/", "mine").unwrap();
    assert!(matches!(result, CasResult::Forked { .. }), "{result:?}");

    // The session remains usable after the fork rather than becoming permanently
    // wedged by an observation that can never match its pinned base.
    assert_eq!(f.read(&root, &a, "/a.txt").unwrap(), b"v0");
    assert_eq!(f.read(&root, &a, "/b.txt").unwrap(), b"mine");
}
