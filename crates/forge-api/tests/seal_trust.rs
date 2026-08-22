use ed25519_dalek::{Signer, SigningKey};
use forge_api::Forge;
use forge_core::{hash_bytes, Commit, Snapshot};
use forge_types::Error;
use tempfile::tempdir;

fn signing_key(dir: &std::path::Path) -> SigningKey {
    let bytes = std::fs::read(dir.join(".forge/keys/seal.ed25519")).unwrap();
    let seed: [u8; 32] = bytes.try_into().unwrap();
    SigningKey::from_bytes(&seed)
}

fn sign_snapshot(sk: &SigningKey, snap: &mut Snapshot) {
    let h = hash_bytes(&snap.encode_unsigned());
    snap.sig = sk.sign(h.as_bytes()).to_bytes();
}

#[test]
fn verify_rejects_seal_table_commit_mismatch() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let main = f.store.meta.get_ref("main").unwrap().unwrap();
    let main_commit = f.store.get_commit(main.oid).unwrap();
    let other = f
        .store
        .put_commit(&Commit {
            tree: main_commit.tree,
            parents: vec![main.oid],
            agent: "other".into(),
            msg: "other".into(),
            ts: 2,
            landmark: false,
        })
        .unwrap();
    let prov = f.store.put_blob_data(b"prov").unwrap();
    let sk = signing_key(d.path());
    let mut snap = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "evil".into(),
        ts: 3,
        prov,
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut snap);
    let snap_oid = f.store.put_snapshot(&snap).unwrap();
    f.store
        .meta
        .commit_seal("evil", snap_oid, other, main_commit.tree, "test")
        .unwrap();

    let err = f.verify_tag(&root, "evil").unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}

#[test]
fn verify_rejects_signed_snapshot_with_wrong_tag_or_provenance_type() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let main = f.store.meta.get_ref("main").unwrap().unwrap();
    let main_commit = f.store.get_commit(main.oid).unwrap();
    let sk = signing_key(d.path());

    let mut wrong_tag = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "different".into(),
        ts: 3,
        prov: f.store.put_blob_data(b"prov").unwrap(),
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut wrong_tag);
    let oid = f.store.put_snapshot(&wrong_tag).unwrap();
    f.store
        .meta
        .commit_seal("tag-check", oid, main.oid, main_commit.tree, "test")
        .unwrap();
    assert!(matches!(
        f.verify_tag(&root, "tag-check"),
        Err(Error::Corrupt(_))
    ));

    let mut wrong_prov = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "prov-check".into(),
        ts: 4,
        // Deliberately point provenance at a Tree object.
        prov: main_commit.tree,
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut wrong_prov);
    let oid = f.store.put_snapshot(&wrong_prov).unwrap();
    f.store
        .meta
        .commit_seal("prov-check", oid, main.oid, main_commit.tree, "test")
        .unwrap();
    assert!(matches!(
        f.verify_tag(&root, "prov-check"),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn verification_bypasses_cached_snapshot_bytes() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let snap_oid = f.seal(&root, "main", "cache-check").unwrap();
    f.verify_tag(&root, "cache-check").unwrap();

    std::fs::write(f.store.blobs.object_path(snap_oid), b"tampered").unwrap();
    let err = f.verify_tag(&root, "cache-check").unwrap_err();
    assert!(matches!(err, Error::Corrupt(_) | Error::NotFound(_)), "{err:?}");
}
