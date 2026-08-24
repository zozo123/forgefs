use ed25519_dalek::{Signer, SigningKey};
use forge_api::Forge;
use forge_core::cbor::{encode_map_sorted, encode_text};
use forge_core::{hash_bytes, Blob, Commit, ProvenanceManifest, Snapshot, Tree, TreeEntry};
use forge_store::Store;
use forge_types::{CasResult, EntryKind, Error, ObjectId};
use std::collections::BTreeMap;
use tempfile::tempdir;

fn root_store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join(".forge")).unwrap()
}

fn signing_key(dir: &std::path::Path) -> SigningKey {
    let bytes = std::fs::read(dir.join(".forge/keys/seal.ed25519")).unwrap();
    let seed: [u8; 32] = bytes.try_into().unwrap();
    SigningKey::from_bytes(&seed)
}

fn sign_snapshot(sk: &SigningKey, snap: &mut Snapshot) {
    let h = hash_bytes(&snap.encode_unsigned());
    snap.sig = sk.sign(h.as_bytes()).to_bytes();
}

fn encode_legacy_provenance(entries: &BTreeMap<ObjectId, String>) -> Vec<u8> {
    let pairs = entries
        .iter()
        .map(|(id, agent)| {
            let mut key = Vec::new();
            encode_text(&mut key, &id.hex());
            let mut value = Vec::new();
            encode_text(&mut value, agent);
            (key, value)
        })
        .collect();
    let mut encoded = Vec::new();
    encode_map_sorted(&mut encoded, pairs);
    encoded
}

fn one_file_tree(store: &Store, name: &str, data: &[u8]) -> forge_types::ObjectId {
    let blob = store.put_blob_data(data).unwrap();
    store
        .put_tree(
            &Tree::new(vec![TreeEntry {
                name: name.into(),
                kind: EntryKind::Blob,
                id: blob,
                exec: false,
            }])
            .unwrap(),
        )
        .unwrap()
}

fn commit(
    store: &Store,
    tree: forge_types::ObjectId,
    parents: Vec<forge_types::ObjectId>,
    n: u64,
) -> forge_types::ObjectId {
    store
        .put_commit(&Commit {
            tree,
            parents,
            agent: "test".into(),
            msg: format!("c{n}"),
            ts: n,
            landmark: false,
            contrib: None,
        })
        .unwrap()
}

#[test]
fn merge_materializes_multiple_best_bases_as_conflict() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let s = root_store(d.path());

    let root_tree = one_file_tree(&s, "root", b"0");
    let root_c = commit(&s, root_tree, vec![], 1);
    let a1 = commit(&s, one_file_tree(&s, "a", b"a"), vec![root_c], 2);
    let b1 = commit(&s, one_file_tree(&s, "b", b"b"), vec![root_c], 3);
    let a2 = commit(&s, one_file_tree(&s, "a2", b"a2"), vec![a1, b1], 4);
    let b2 = commit(&s, one_file_tree(&s, "b2", b"b2"), vec![b1, a1], 5);
    s.meta
        .insert_ref("criss/a", a2, "commit", false, false, "test", "test")
        .unwrap();
    s.meta
        .insert_ref("criss/b", b2, "commit", false, false, "test", "test")
        .unwrap();

    let err = f.merge(&root, "criss/a", "criss/b", None).unwrap_err();
    let Error::MergeConflict(conflict_oid) = err else {
        panic!("expected explicit structural conflict, got {err:?}");
    };
    let conflict = s.get_conflict(conflict_oid).unwrap();
    assert_eq!(conflict.bases.len(), 2);
    assert_eq!(conflict.causal, vec![a2, b2]);
}

#[test]
fn verify_rejects_metadata_commit_tag_and_provenance_mismatch() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let s = root_store(d.path());
    let main = s.meta.get_ref("main").unwrap().unwrap();
    let main_commit = s.get_commit(main.oid).unwrap();
    let sk = signing_key(d.path());

    let other_commit = commit(&s, main_commit.tree, vec![main.oid], 9);
    let mut bad_commit = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "bad-commit".into(),
        ts: 10,
        prov: s.put_blob_data(b"prov").unwrap(),
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut bad_commit);
    let oid = s.put_snapshot(&bad_commit).unwrap();
    s.meta
        .commit_seal("bad-commit", oid, other_commit, main_commit.tree, "test")
        .unwrap();
    assert!(matches!(
        f.verify_tag(&root, "bad-commit"),
        Err(Error::Corrupt(_))
    ));

    let mut bad_tag = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "different".into(),
        ts: 11,
        prov: s.put_blob_data(b"prov").unwrap(),
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut bad_tag);
    let oid = s.put_snapshot(&bad_tag).unwrap();
    s.meta
        .commit_seal("bad-tag", oid, main.oid, main_commit.tree, "test")
        .unwrap();
    assert!(matches!(
        f.verify_tag(&root, "bad-tag"),
        Err(Error::Corrupt(_))
    ));

    let mut bad_prov = Snapshot {
        tree: main_commit.tree,
        commit: main.oid,
        tag: "bad-prov".into(),
        ts: 12,
        prov: main_commit.tree,
        pk: sk.verifying_key().to_bytes(),
        sig: [0; 64],
    };
    sign_snapshot(&sk, &mut bad_prov);
    let oid = s.put_snapshot(&bad_prov).unwrap();
    s.meta
        .commit_seal("bad-prov", oid, main.oid, main_commit.tree, "test")
        .unwrap();
    assert!(matches!(
        f.verify_tag(&root, "bad-prov"),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn verify_and_full_fsck_reject_inexact_or_forged_provenance_manifest() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let ns = f.session_open(&root, "main").unwrap();
    f.write(&root, &ns, "/receipt.txt", b"causal", false)
        .unwrap();
    let contribution_ref = match f.checkin(&root, &ns, "/", "receipt").unwrap() {
        CasResult::Updated { name, .. } => name,
        other => panic!("material checkin did not update its private ref: {other:?}"),
    };
    f.merge(&root, "main", &contribution_ref, None).unwrap();
    let original_oid = f.seal(&root, "main", "complete-manifest").unwrap();

    let store = root_store(d.path());
    let contribution_commit = store.meta.get_ref(&contribution_ref).unwrap().unwrap().oid;
    let contribution_oid = store
        .get_commit(contribution_commit)
        .unwrap()
        .contrib
        .unwrap();
    let original = store.get_snapshot(original_oid).unwrap();
    let provenance = Blob::decode(&store.get_raw_verified(original.prov).unwrap()).unwrap();
    let entries = ProvenanceManifest::decode(&provenance.data)
        .unwrap()
        .entries()
        .clone();
    let sk = signing_key(d.path());
    let publish = |tag: &str, entries| {
        let mut altered = original.clone();
        altered.tag = tag.into();
        altered.ts += tag.len() as u64;
        altered.prov = store
            .put_blob_data(&ProvenanceManifest::new(entries).unwrap().encode())
            .unwrap();
        sign_snapshot(&sk, &mut altered);
        let oid = store.put_snapshot(&altered).unwrap();
        store
            .meta
            .commit_seal(tag, oid, altered.commit, altered.tree, "test")
            .unwrap();
    };

    let mut missing = entries.clone();
    assert!(missing.remove(&contribution_oid).is_some());
    publish("missing-contribution", missing);

    let mut misattributed = entries.clone();
    misattributed.insert(contribution_oid, "forged-agent".into());
    publish("misattributed-contribution", misattributed);

    let unreachable = store
        .put_blob_data(b"unreachable provenance entry")
        .unwrap();
    let mut extra = entries;
    extra.insert(unreachable, "forged-agent".into());
    publish("extra-provenance", extra);

    let error = f.verify_tag(&root, "missing-contribution").unwrap_err();
    assert!(
        matches!(&error, Error::Corrupt(detail) if detail.contains("provenance missing reachable object") && detail.contains(&contribution_oid.hex())),
        "{error:?}"
    );
    let error = f
        .verify_tag(&root, "misattributed-contribution")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Corrupt(detail) if detail.contains("provenance contribution attribution mismatch") && detail.contains(&contribution_oid.hex())),
        "{error:?}"
    );
    let error = f.verify_tag(&root, "extra-provenance").unwrap_err();
    assert!(
        matches!(&error, Error::Corrupt(detail) if detail.contains("provenance contains unreachable object") && detail.contains(&unreachable.hex())),
        "{error:?}"
    );
    let report = f.fsck(&root, true).unwrap();
    assert!(!report.ok);
    for tag in [
        "missing-contribution",
        "misattributed-contribution",
        "extra-provenance",
    ] {
        assert!(report.findings.iter().any(|finding| {
            finding.code == "SEAL_PAYLOAD" && finding.resource == format!("catalog:seal:{tag}")
        }));
    }
}

#[test]
fn legacy_content_only_seal_with_contribution_history_remains_verifiable() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let ns = f.session_open(&root, "main").unwrap();
    f.write(&root, &ns, "/legacy.txt", b"receipt", false)
        .unwrap();
    let contribution_ref = match f.checkin(&root, &ns, "/", "legacy receipt").unwrap() {
        CasResult::Updated { name, .. } => name,
        other => panic!("material checkin did not update its private ref: {other:?}"),
    };
    f.merge(&root, "main", &contribution_ref, None).unwrap();
    let current_oid = f.seal(&root, "main", "current-format").unwrap();

    let store = root_store(d.path());
    let current = store.get_snapshot(current_oid).unwrap();
    let current_provenance = Blob::decode(&store.get_raw_verified(current.prov).unwrap()).unwrap();
    let mut legacy_entries = ProvenanceManifest::decode(&current_provenance.data)
        .unwrap()
        .entries()
        .clone();
    let contribution_commit = store.meta.get_ref(&contribution_ref).unwrap().unwrap().oid;
    let contribution_oid = store
        .get_commit(contribution_commit)
        .unwrap()
        .contrib
        .unwrap();
    assert!(legacy_entries.remove(&contribution_oid).is_some());

    let mut legacy = current;
    legacy.tag = "legacy-format".into();
    legacy.ts += 1;
    legacy.prov = store
        .put_blob_data(&encode_legacy_provenance(&legacy_entries))
        .unwrap();
    sign_snapshot(&signing_key(d.path()), &mut legacy);
    let legacy_oid = store.put_snapshot(&legacy).unwrap();
    store
        .meta
        .commit_seal(&legacy.tag, legacy_oid, legacy.commit, legacy.tree, "test")
        .unwrap();

    assert_eq!(f.verify_tag(&root, &legacy.tag).unwrap(), legacy_oid);
    let report = f.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
}

#[test]
fn verification_bypasses_cached_snapshot_bytes() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let snap_oid = f.seal(&root, "main", "cache-check").unwrap();
    f.verify_tag(&root, "cache-check").unwrap();

    let s = root_store(d.path());
    std::fs::write(s.blobs.object_path(snap_oid), b"tampered").unwrap();
    let err = f.verify_tag(&root, "cache-check").unwrap_err();
    assert!(
        matches!(err, Error::Corrupt(_) | Error::NotFound(_)),
        "{err:?}"
    );
}

#[cfg(unix)]
#[test]
fn key_material_has_strict_permissions_and_open_rejects_loose_secret() {
    use std::os::unix::fs::PermissionsExt;

    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    drop(f);
    let keys = d.path().join(".forge/keys");
    assert_eq!(
        std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["root.secret", "seal.ed25519", "root.cap", "integrator.cap"] {
        let p = keys.join(name);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600,
            "{name}"
        );
    }

    let secret = keys.join("root.secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(Forge::open(d.path()), Err(Error::Denied(_))));
}
