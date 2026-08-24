use forge_api::Forge;
use forge_types::Error;
use tempfile::tempdir;

fn agent(forge: &Forge, name: &str) -> forge_cap::Cap {
    let root = forge.root_cap().unwrap();
    forge
        .grant(
            &root,
            vec![
                "ops=read,write,branch".into(),
                format!("agent={name}"),
                format!("allow=read:main,heads/agents/{name}/*"),
                format!("allow=write:heads/agents/{name}/*"),
                format!("allow=branch:heads/agents/{name}/*"),
            ],
        )
        .unwrap()
}

#[test]
fn integrator_can_discover_and_read_conflict_ref() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let alice = agent(&forge, "alice");
    let bob = agent(&forge, "bob");
    let integrator = forge.integrator_cap().unwrap();

    let a = forge.session_open(&alice, "main").unwrap();
    let b = forge.session_open(&bob, "main").unwrap();
    forge.write(&alice, &a, "/x.txt", b"alice", false).unwrap();
    forge.write(&bob, &b, "/x.txt", b"bob", false).unwrap();
    forge.checkin(&alice, &a, "/", "alice").unwrap();
    forge.checkin(&bob, &b, "/", "bob").unwrap();

    let a_ref = format!("heads/agents/alice/{a}");
    let b_ref = format!("heads/agents/bob/{b}");
    forge.merge(&integrator, "main", &a_ref, None).unwrap();
    let conflict_oid = match forge.merge(&integrator, "main", &b_ref, None) {
        Err(Error::MergeConflict(oid)) => oid,
        other => panic!("expected conflict, got {other:?}"),
    };

    let (refs, suppressed) = forge.refs_with_suppressed(&integrator).unwrap();
    assert_eq!(suppressed, 0, "integrator should see built-in merge refs");
    let conflict_ref = refs
        .iter()
        .find(|row| row.kind == "conflict" && row.oid == conflict_oid)
        .expect("conflict ref must be visible to integrator");
    let shown = forge.show(&integrator, &conflict_ref.name).unwrap();
    assert!(shown.contains(&conflict_oid.hex()), "{shown}");
}

#[test]
fn ref_enumeration_reports_how_many_rows_authority_suppressed() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "root-only").unwrap();

    let integrator = forge.integrator_cap().unwrap();
    let (refs, suppressed) = forge.refs_with_suppressed(&integrator).unwrap();
    assert!(refs.iter().any(|row| row.name == "main"));
    assert!(!refs.iter().any(|row| row.name == "root-only"));
    assert_eq!(suppressed, 1);
}
