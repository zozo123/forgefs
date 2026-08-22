use forge_api::Forge;
use forge_types::CasResult;
use tempfile::tempdir;

// Inbox tests are capability-boundary regressions, not only happy-path API tests.
#[test]
fn sealed_snapshot_can_be_published_to_recipient_inbox() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let snap = forge.seal(&root, "main", "v1.0").unwrap();
    let alice = forge
        .grant(
            &root,
            vec![
                "ops=read,write".into(),
                "agent=alice".into(),
                "ref=tags/v1.0,inbox/bob/*".into(),
            ],
        )
        .unwrap();
    let bob = forge
        .grant(
            &root,
            vec![
                "ops=read".into(),
                "agent=bob".into(),
                "ref=inbox/bob/*".into(),
            ],
        )
        .unwrap();
    let result = forge.inbox_push(&alice, "bob", "tags/v1.0").unwrap();
    let CasResult::Updated { name, oid } = result else {
        panic!("new inbox ref must publish directly");
    };
    assert!(name.starts_with("inbox/bob/"));
    assert_eq!(oid, snap);
    let rows = forge.inbox_list(&bob).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, name);
    assert_eq!(rows[0].oid, snap);
    assert_eq!(rows[0].kind, "snapshot");
}

#[test]
fn inbox_write_requires_concrete_prefix_authority() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "v1.0").unwrap();
    let alice = forge
        .grant(
            &root,
            vec![
                "ops=read,write".into(),
                "agent=alice".into(),
                "ref=tags/v1.0,inbox/alice/*".into(),
            ],
        )
        .unwrap();
    assert!(forge.inbox_push(&alice, "bob", "tags/v1.0").is_err());
}

#[test]
fn invalid_inbox_recipient_fails_closed() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "v1.0").unwrap();
    assert!(forge.inbox_push(&root, "../bob", "tags/v1.0").is_err());
}

#[test]
fn inbox_list_requires_read_authority() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let write_only = forge
        .grant(
            &root,
            vec![
                "ops=write".into(),
                "agent=bob".into(),
                "ref=inbox/bob/*".into(),
            ],
        )
        .unwrap();
    assert!(forge.inbox_list(&write_only).is_err());
}
