//! I17: the repository VERSION gates immutable decoding and is independent of
//! the SQLite schema version. A metadata migration must move mutable rows
//! forward while leaving every immutable object byte -- and therefore every
//! ObjectId -- exactly where it was.
//!
//! The migration under test is the real one the code implements: a catalog is
//! returned to schema version 0 by removing the `schema_migrations` ledger,
//! which is precisely how `schema_version()` defines a pre-versioning catalog,
//! and the next writable open must migrate it forward.

use forge_api::Forge;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// Every immutable byte under `.forge/objects`, keyed by repository-relative
/// path. Path and content are both content-addressed, so any rewrite of an
/// object or of an ObjectId shows up here.
fn object_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let objects = root.join("objects");
    let mut found = BTreeMap::new();
    let mut stack = vec![objects.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read objects dir") {
            let entry = entry.expect("objects dir entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                stack.push(path);
            } else {
                let key = path
                    .strip_prefix(&objects)
                    .expect("object path is under objects/")
                    .to_path_buf();
                found.insert(key, fs::read(&path).expect("read object"));
            }
        }
    }
    assert!(
        !found.is_empty(),
        "the fixture repository must have objects"
    );
    found
}

fn ref_oids(forge: &Forge) -> Vec<(String, String)> {
    let cap = forge.root_cap().expect("root cap");
    let mut rows: Vec<(String, String)> = forge
        .refs(&cap)
        .expect("refs")
        .into_iter()
        .map(|row| (row.name, row.oid.hex()))
        .collect();
    rows.sort();
    rows
}

#[test]
fn migrating_the_catalog_never_rewrites_objects_or_object_ids() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("src");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("a.txt"), b"alpha\n").unwrap();
    fs::write(source.join("nested/b.txt"), b"beta\n").unwrap();

    let forge = Forge::init(dir.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    forge.import_dir(&cap, &source, "heads/main").unwrap();
    let root = forge.root().to_path_buf();
    let before_objects = object_bytes(&root);
    let before_refs = ref_oids(&forge);
    let before_version = fs::read(root.join("VERSION")).unwrap();
    drop(forge);

    // Return the catalog to schema version 0: no ledger, every row intact.
    let conn = Connection::open(root.join("meta.sqlite")).unwrap();
    conn.execute_batch("DROP TABLE schema_migrations;").unwrap();
    drop(conn);

    let forge = Forge::open(dir.path()).expect("a version-0 catalog must migrate on open");
    assert_eq!(
        ref_oids(&forge),
        before_refs,
        "migration must not move a single ref ObjectId"
    );
    let report = forge.fsck(&forge.root_cap().unwrap(), true).unwrap();
    assert!(
        report.ok && report.findings.is_empty(),
        "a migrated repository must fsck clean: {:?}",
        report.findings
    );
    assert!(report.checked_objects > 0);
    drop(forge);

    assert_eq!(
        object_bytes(&root),
        before_objects,
        "a metadata migration must not add, remove, or rewrite immutable objects"
    );
    assert_eq!(
        fs::read(root.join("VERSION")).unwrap(),
        before_version,
        "the repository VERSION is independent of the SQLite schema version"
    );

    let conn = Connection::open(root.join("meta.sqlite")).unwrap();
    let ledger: Vec<i64> = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ledger, vec![forge_store::CURRENT_SCHEMA_VERSION]);
}
