use forge_api::Forge;
use forge_types::Error;
use std::fs;
use tempfile::tempdir;

#[test]
fn init_publishes_only_a_complete_versioned_repository() {
    let dir = tempdir().unwrap();
    let stale = dir.path().join(".forge.init-dead-worker");
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("junk"), b"partial").unwrap();

    let forge = Forge::init(dir.path()).unwrap();
    assert_eq!(fs::read(forge.root().join("VERSION")).unwrap(), b"1\n");
    assert!(!forge.root().join("config.toml").exists());
    assert!(forge.root().join("keys/root.cap").exists());
    assert!(forge.root().join("meta.sqlite").exists());
    forge.root_cap().unwrap();
    drop(forge);

    // A pre-publication crash leaves only a sibling staging directory; it does
    // not poison discovery or prevent a fully initialized repository opening.
    assert!(stale.join("junk").exists());
    drop(Forge::open(dir.path()).unwrap());
}

#[test]
fn init_never_overwrites_an_unversioned_dot_forge() {
    let dir = tempdir().unwrap();
    let root = dir.path().join(".forge");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sentinel"), b"keep").unwrap();
    let error = Forge::init(dir.path()).err().expect("must fail closed");
    assert!(matches!(error, Error::Invalid(_)), "{error}");
    assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn future_or_malformed_repository_version_fails_closed() {
    for bytes in [
        b"2\n".as_slice(),
        b"garbage\n".as_slice(),
        b"1  ".as_slice(),
    ] {
        let dir = tempdir().unwrap();
        drop(Forge::init(dir.path()).unwrap());
        fs::write(dir.path().join(".forge/VERSION"), bytes).unwrap();
        let error = Forge::open(dir.path())
            .err()
            .expect("unsupported VERSION must fail");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
    }
}

#[test]
fn import_excludes_only_root_control_directories() {
    let source = tempdir().unwrap();
    fs::create_dir_all(source.path().join(".git")).unwrap();
    fs::write(source.path().join(".git/root-control"), b"skip").unwrap();
    fs::create_dir_all(source.path().join("nested/.git")).unwrap();
    fs::write(source.path().join("nested/.git/keep"), b"keep").unwrap();

    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    forge
        .import_dir(&cap, source.path(), "heads/import")
        .unwrap();
    let out = dir.path().join("import.tar");
    forge.export_tar(&cap, "heads/import", &out).unwrap();

    let file = fs::File::open(out).unwrap();
    let mut archive = tar::Archive::new(file);
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string()
        })
        .collect();
    assert!(
        names.iter().any(|name| name == "nested/.git/keep"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|name| name == ".git/root-control"),
        "{names:?}"
    );
}
