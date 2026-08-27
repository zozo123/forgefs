//! I15: staged export may consume a Blob before its final hash comparison, but
//! corrupt bytes must never become a published archive.

use forge_api::Forge;
use forge_types::ObjectId;
use std::fs;
use tempfile::tempdir;

#[test]
fn i15_late_blob_corruption_never_publishes_a_streamed_export() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    f.branch(&root, "main", "work").unwrap();
    let ns = f.session_open(&root, "work").unwrap();
    f.mount(&root, &ns, "/", "ref:work", true).unwrap();
    f.write(&root, &ns, "/large.bin", &vec![0x5a; 256 * 1024], false)
        .unwrap();
    f.checkin(&root, &ns, "/", "seed").unwrap();

    let oid_hex = f
        .ls(&root, &ns, "/")
        .unwrap()
        .into_iter()
        .find(|(name, kind, _, _)| name == "large.bin" && kind == "blob")
        .expect("checked-in blob must be listed")
        .2;
    let oid = ObjectId::from_hex(&oid_hex).unwrap();
    let (a, b) = oid.shard_dirs();
    let object = f.root().join("objects").join(a).join(b).join(oid.hex());

    // Flip the last payload byte, not the frame. The staged reader therefore
    // writes the full tar member and discovers corruption only in finish().
    let mut bytes = fs::read(&object).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&object, bytes).unwrap();

    let out = d.path().join("corrupt.tar");
    let err = f.export_tar(&root, "work", &out).unwrap_err().to_string();
    assert!(
        err.contains("hash mismatch"),
        "unexpected export error: {err}"
    );
    assert!(
        !out.exists(),
        "corrupt staged bytes reached the final archive"
    );
    let prefix = out.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .count(),
        0,
        "failed streamed export left a sibling partial artifact"
    );
}
