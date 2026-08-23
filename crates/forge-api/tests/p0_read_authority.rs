use forge_api::Forge;
use forge_types::Error;
use tempfile::tempdir;

#[test]
fn read_only_cap_cannot_open_writable_session() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let read_only = f
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "allow=read:main".into(),
                "agent=reader".into(),
            ],
        )
        .unwrap();

    let err = f.session_open(&read_only, "main").unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

#[test]
fn show_obeys_concrete_ref_scope_and_raw_oid_policy() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "public").unwrap();
    f.branch(&root, "main", "secret").unwrap();

    let public = f
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "allow=read:public".into(),
                "agent=reader".into(),
            ],
        )
        .unwrap();

    assert!(f.show(&public, "public").is_ok());
    let err = f.show(&public, "secret").unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    let main_oid = f.peel_commit("main").unwrap().0;
    let raw = format!("oid:{}", main_oid.hex());
    let err = f.show(&public, &raw).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    assert!(f.show(&root, &raw).is_ok());
}

#[test]
fn negative_ref_scope_cannot_bypass_authority_with_raw_oids() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let main_oid = f.peel_commit("main").unwrap().0;
    let raw = format!("oid:{}", main_oid.hex());
    let ns = f.session_open(&root, "main").unwrap();

    let excluding_main = f
        .grant(&root, vec!["ops=read,write".into(), "ref!=main".into()])
        .unwrap();

    assert!(matches!(f.show(&excluding_main, "main"), Err(Error::Denied(_))));
    assert!(matches!(f.show(&excluding_main, &raw), Err(Error::Denied(_))));
    assert!(matches!(f.mount(&excluding_main, &ns, "/raw-ro", &raw, false), Err(Error::Denied(_))));
    assert!(matches!(f.mount(&excluding_main, &ns, "/raw-rw", &raw, true), Err(Error::Denied(_))));

    let export = d.path().join("excluded.tar");
    assert!(matches!(f.export_tar(&excluding_main, &raw, &export), Err(Error::Denied(_))));
    assert!(!export.exists());
    assert!(matches!(f.landmark(&excluding_main, main_oid), Err(Error::Denied(_))));
    assert!(matches!(f.fsck(&excluding_main, false), Err(Error::Denied(_))));

    assert!(f.show(&root, &raw).is_ok());
}

#[test]
fn scoped_agent_can_open_its_authorized_live_ref() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let alice = f
        .grant(
            &root,
            vec![
                "ops=read,write,branch".into(),
                "agent=alice".into(),
                "allow=read:main,heads/agents/alice/*".into(),
                "allow=write:heads/agents/alice/*".into(),
                "allow=branch:heads/agents/alice/*".into(),
            ],
        )
        .unwrap();

    let ns = f.session_open(&alice, "main").unwrap();
    f.write(&alice, &ns, "/x.txt", b"x", false).unwrap();
}
