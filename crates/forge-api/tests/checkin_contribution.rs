use forge_api::Forge;
use forge_types::{CasResult, ObjectId};
use tempfile::tempdir;

fn updated_oid(result: CasResult) -> ObjectId {
    match result {
        CasResult::Updated { oid, .. } | CasResult::Forked { ours: oid, .. } => oid,
        CasResult::Noop { .. } => panic!("expected a material checkin"),
    }
}

#[test]
fn checkin_persists_showable_contribution_from_reads_and_writes() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    forge.write(&root, &ns, "/a.txt", b"a", false).unwrap();
    let first_oid = updated_oid(forge.checkin(&root, &ns, "/", "first").unwrap());
    let (_, first) = forge.peel_commit(&format!("oid:{}", first_oid)).unwrap();
    let first_contrib = first.contrib.expect("material checkin must have a receipt");
    let first_show = forge.show(&root, &format!("oid:{first_contrib}")).unwrap();
    assert!(first_show.contains("write /a.txt"));
    assert!(first_show.contains(&format!("tree {}", first.tree)));

    assert_eq!(forge.read(&root, &ns, "/a.txt").unwrap(), b"a");
    forge.write(&root, &ns, "/b.txt", b"b", false).unwrap();
    let second_oid = updated_oid(forge.checkin(&root, &ns, "/", "second").unwrap());
    let (_, second) = forge.peel_commit(&format!("oid:{}", second_oid)).unwrap();
    let second_contrib = second
        .contrib
        .expect("material checkin must have a receipt");
    let shown = forge.show(&root, &format!("oid:{second_contrib}")).unwrap();

    assert!(shown.contains("read "));
    assert!(shown.contains(" /a.txt"));
    assert!(shown.contains("write /b.txt"));
    assert!(shown.contains(&format!("base {}", first_oid)));
    assert!(shown.contains(&format!("tree {}", second.tree)));
    assert!(shown
        .lines()
        .any(|line| line.starts_with("agent ") && line.len() > 6));
}

#[test]
fn noop_checkin_does_not_invent_a_receipt() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    assert!(matches!(
        forge.checkin(&root, &ns, "/", "noop").unwrap(),
        CasResult::Noop { .. }
    ));
}
