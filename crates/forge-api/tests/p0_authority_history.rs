use forge_api::Forge;
use forge_types::{CasResult, Error, ObjectId};
use std::fs;
use tempfile::tempdir;

#[test]
fn branch_and_merge_require_read_authority_on_source() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    f.branch(&root, "main", "secret").unwrap();
    f.branch(&root, "main", "dest").unwrap();

    let branch_cap = f
        .grant(
            &root,
            vec![
                "ops=read,branch".into(),
                "allow=read:public".into(),
                "allow=branch:public".into(),
            ],
        )
        .unwrap();

    let err = f.branch(&branch_cap, "secret", "public").unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    let main_oid = f.peel_commit("main").unwrap().0;
    let err = f
        .branch(&branch_cap, &main_oid.hex(), "public")
        .unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    let merge_cap = f
        .grant(
            &root,
            vec![
                "ops=read,write".into(),
                "allow=read:dest".into(),
                "allow=write:dest".into(),
            ],
        )
        .unwrap();

    let err = f.merge(&merge_cap, "dest", "secret", None).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

#[test]
fn repeated_import_preserves_commit_ancestry() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let src = d.path().join("source");
    fs::create_dir(&src).unwrap();

    fn updated_oid(result: CasResult) -> ObjectId {
        match result {
            CasResult::Updated { oid, .. } => oid,
            other => panic!("sequential import unexpectedly did not update: {other:?}"),
        }
    }

    fs::write(src.join("data.txt"), b"v1").unwrap();
    let first = updated_oid(f.import_dir(&root, &src, "imports/test").unwrap());
    let (_, c1) = f.peel_commit("imports/test").unwrap();
    assert!(c1.parents.is_empty());

    fs::write(src.join("data.txt"), b"v2").unwrap();
    let second = updated_oid(f.import_dir(&root, &src, "imports/test").unwrap());
    assert_ne!(first, second);
    let (head, c2) = f.peel_commit("imports/test").unwrap();
    assert_eq!(head, second);
    assert_eq!(c2.parents, vec![first]);
}
