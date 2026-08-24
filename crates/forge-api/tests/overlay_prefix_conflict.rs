// Regression coverage for #273: one staged overlay must map to one representable tree.
use forge_api::Forge;
use forge_types::Error;
use tempfile::tempdir;

fn assert_invalid<T>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::Invalid(_))));
}

#[test]
fn staged_file_then_descendant_is_rejected_and_file_survives() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    forge.write(&root, &ns, "/node", b"parent", false).unwrap();
    assert_invalid(forge.write(&root, &ns, "/node/child", b"child", false));
    forge.checkin(&root, &ns, "/", "parent only").unwrap();

    assert_eq!(forge.read(&root, &ns, "/node").unwrap(), b"parent");
}

#[test]
fn staged_descendant_then_file_is_rejected_and_descendant_survives() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    forge
        .write(&root, &ns, "/node/child", b"child", false)
        .unwrap();
    assert_invalid(forge.write(&root, &ns, "/node", b"parent", false));
    forge.checkin(&root, &ns, "/", "child only").unwrap();

    assert_eq!(forge.read(&root, &ns, "/node/child").unwrap(), b"child");
}

#[test]
fn ancestor_tombstone_and_descendant_write_cannot_coexist() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    forge.delete(&root, &ns, "/node").unwrap();
    assert_invalid(forge.write(&root, &ns, "/node/child", b"child", false));
}

#[test]
fn deleting_mount_root_is_rejected() {
    let d = tempdir().unwrap();
    let forge = Forge::init(d.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();

    assert_invalid(forge.delete(&root, &ns, "/"));
}
