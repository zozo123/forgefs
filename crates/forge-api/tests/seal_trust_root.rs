use forge_api::Forge;
use forge_store::Store;
use forge_types::Error;
use tempfile::tempdir;

#[test]
fn mutable_metadata_cannot_replace_the_local_seal_trust_root() {
    let d = tempdir().unwrap();
    drop(Forge::init(d.path()).unwrap());

    let store = Store::open(&d.path().join(".forge")).unwrap();
    store.meta.set_cap_root(&[7u8; 32]).unwrap();
    drop(store);

    let err = Forge::open(d.path()).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}
