use forge_api::Forge;
use forge_core::{Commit, Contribution, ContributionRead};
use forge_store::Store;
use tempfile::tempdir;

#[test]
fn clean_init_and_seal_pass_reachable_and_full_fsck() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    let r = f.fsck(&root, false).unwrap();
    assert!(r.ok, "{:#?}", r.findings);
    assert!(r.checked_refs >= 1);
    assert!(r.checked_objects >= 2);

    f.seal(&root, "main", "fsck-clean").unwrap();
    let r = f.fsck(&root, true).unwrap();
    assert!(r.ok, "{:#?}", r.findings);
    assert!(r.checked_refs >= 2);
}

#[test]
fn durable_fsck_detects_corruption_even_after_cache_warmup() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.show(&root, "main").unwrap();

    let store = Store::open(&d.path().join(".forge")).unwrap();
    let main = store.meta.get_ref("main").unwrap().unwrap();
    std::fs::write(store.blobs.object_path(main.oid), b"tampered").unwrap();

    let r = f.fsck(&root, false).unwrap();
    assert!(!r.ok);
    assert!(
        r.findings.iter().any(|f| f.code == "OBJECT_READ"),
        "{:#?}",
        r.findings
    );
}

#[test]
fn typed_commit_tree_edge_is_checked() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let store = Store::open(&d.path().join(".forge")).unwrap();

    let blob = store.put_blob_data(b"not-a-tree").unwrap();
    let bad_commit = store
        .put_commit(&Commit {
            tree: blob,
            parents: vec![],
            agent: "test".into(),
            msg: "bad tree edge".into(),
            ts: 1,
            landmark: false,
            contrib: None,
        })
        .unwrap();
    store
        .meta
        .insert_ref(
            "heads/bad-tree",
            bad_commit,
            "commit",
            false,
            false,
            "test",
            "fsck-test",
        )
        .unwrap();

    let r = f.fsck(&root, false).unwrap();
    assert!(!r.ok);
    assert!(
        r.findings
            .iter()
            .any(|f| { f.code == "TYPE_MISMATCH" && f.resource.contains("commit:") }),
        "{:#?}",
        r.findings
    );
}

#[test]
fn contribution_read_edges_must_reference_blobs() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let store = Store::open(&d.path().join(".forge")).unwrap();

    let main = store.meta.get_ref("main").unwrap().unwrap();
    let base = store.get_commit(main.oid).unwrap();
    let contribution = store
        .put_contribution(&Contribution {
            base: main.oid,
            tree: base.tree,
            parents: vec![],
            reads: vec![ContributionRead {
                path: "/not-a-blob".into(),
                id: base.tree,
            }],
            writes: vec![],
            agent: "test".into(),
            ts: 1,
        })
        .unwrap();
    let bad_commit = store
        .put_commit(&Commit {
            tree: base.tree,
            parents: vec![main.oid],
            agent: "test".into(),
            msg: "bad contribution read edge".into(),
            ts: 2,
            landmark: false,
            contrib: Some(contribution),
        })
        .unwrap();
    store
        .meta
        .insert_ref(
            "heads/bad-contribution-read",
            bad_commit,
            "commit",
            false,
            false,
            "test",
            "fsck-test",
        )
        .unwrap();

    let report = f.fsck(&root, false).unwrap();
    assert!(!report.ok);
    assert!(
        report.findings.iter().any(|finding| {
            finding.code == "TYPE_MISMATCH"
                && finding.resource.contains("contribution:")
                && finding.resource.contains(":read:/not-a-blob")
        }),
        "{:#?}",
        report.findings
    );
}

#[test]
fn full_fsck_reports_malformed_orphan_layout() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let bad_dir = d.path().join(".forge/objects/zz/zz");
    std::fs::create_dir_all(&bad_dir).unwrap();
    std::fs::write(bad_dir.join("not-an-object-id"), b"orphan").unwrap();

    let reachable = f.fsck(&root, false).unwrap();
    assert!(reachable.ok, "{:#?}", reachable.findings);

    let full = f.fsck(&root, true).unwrap();
    assert!(!full.ok);
    assert!(
        full.findings
            .iter()
            .any(|f| f.code == "OBJECT_NAME" || f.code == "OBJECT_SHARD"),
        "{:#?}",
        full.findings
    );
}

#[test]
fn fsck_requires_unrestricted_read_authority() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let scoped = f
        .grant(
            &root,
            vec!["ops=read".into(), "ref=main".into(), "agent=a".into()],
        )
        .unwrap();
    assert!(f.fsck(&scoped, false).is_err());
}
