use forge_api::Forge;
use std::fs;

#[test]
fn init_writes_repository_format_version_one() {
    let dir = tempfile::tempdir().unwrap();
    let _forge = Forge::init(dir.path()).unwrap();
    assert_eq!(fs::read(dir.path().join(".forge/VERSION")).unwrap(), b"1\n");
}
