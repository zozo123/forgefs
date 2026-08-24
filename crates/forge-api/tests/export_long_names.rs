//! A tree that imports at exit 0 and passes fsck must be exportable.
//!
//! `validate_name` accepts names up to 255 UTF-8 bytes, but export built ustar
//! headers and called `set_path`, whose name field is 100 bytes. So ForgeFS
//! accepted trees it could not round-trip: a 101-byte file name failed, and a
//! 100-byte directory component failed with the baffling "paths in archives
//! must have at least one component". Worse, the half-written output was left
//! behind as a syntactically valid EMPTY tar.
//!
//! INVARIANTS.md I2 makes losslessness the point, so this asserts a real
//! round-trip: export, extract with the system `tar`, re-import, compare root
//! OIDs byte for byte.

use forge_api::Forge;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn long(n: usize, c: char) -> String {
    std::iter::repeat_n(c, n).collect()
}

#[test]
fn export_round_trips_names_longer_than_the_ustar_name_field() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    // A 101-byte file name (one past the ustar field), a 255-byte name (the
    // documented maximum), and a long directory component, which took the
    // branch that had no fallback at all.
    let src = d.path().join("src");
    let deep = src.join(long(120, 'd'));
    fs::create_dir_all(&deep).unwrap();
    fs::write(src.join(long(101, 'f')), b"just-over").unwrap();
    fs::write(src.join(long(255, 'g')), b"at-the-max").unwrap();
    fs::write(deep.join("inside.txt"), b"nested").unwrap();
    fs::write(src.join("short.txt"), b"ordinary").unwrap();

    f.import_dir(&root, &src, "heads/long").unwrap();
    let report = f.fsck(&root, true).unwrap();
    assert!(report.ok, "{:?}", report.findings);

    let tar_path = d.path().join("out.tar");
    f.export_tar(&root, "heads/long", &tar_path)
        .expect("export must handle every name validate_name accepts");

    // The system tar must be able to read it, not just our own writer.
    let listed = Command::new("tar")
        .arg("-tf")
        .arg(&tar_path)
        .output()
        .expect("run tar");
    assert!(listed.status.success(), "tar -tf failed: {listed:?}");
    let listing = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listing.contains(&long(101, 'f')),
        "101-byte name missing from the archive"
    );
    assert!(
        listing.contains(&long(255, 'g')),
        "255-byte name missing from the archive"
    );
    assert!(
        listing.contains(&long(120, 'd')),
        "long directory component missing from the archive"
    );

    // I2: extract with a real tar and re-import; the root OID must be identical.
    let back = d.path().join("back");
    fs::create_dir_all(&back).unwrap();
    let extracted = Command::new("tar")
        .arg("-xf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&back)
        .output()
        .expect("run tar");
    assert!(extracted.status.success(), "tar -xf failed: {extracted:?}");

    f.import_dir(&root, &back, "heads/long-back").unwrap();

    // Compare the content-addressed listings rather than the commit OIDs:
    // commits embed a timestamp and message, so two imports of identical bytes
    // never share a commit OID. Blob OIDs are content-addressed, so equal
    // listings mean equal bytes, kinds and exec bits.
    let before = listing_of(&f, &root, "heads/long");
    let after = listing_of(&f, &root, "heads/long-back");
    assert_eq!(
        before, after,
        "export/extract/import is not lossless for long names"
    );
    assert!(
        before.len() >= 4,
        "listing looks empty, so the comparison proved nothing: {before:?}"
    );
}

/// Every entry reachable from a ref, as (path, kind, oid, exec), sorted.
fn listing_of(f: &Forge, cap: &forge_cap::Cap, r#ref: &str) -> Vec<(String, String, String, bool)> {
    let ns = f.session_open(cap, r#ref).unwrap();
    f.mount(
        cap,
        &ns,
        "/",
        &format!("ref:{ref_name}", ref_name = r#ref),
        false,
    )
    .unwrap();
    let mut out = Vec::new();
    let mut stack = vec!["/".to_string()];
    while let Some(dir) = stack.pop() {
        for (name, kind, oid, exec) in f.ls(cap, &ns, &dir).unwrap() {
            let child = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            if kind == "tree" {
                stack.push(child.clone());
            }
            out.push((child, kind, oid, exec));
        }
    }
    out.sort();
    out
}

#[test]
fn a_failed_export_leaves_no_artifact_behind() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    let src = d.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), b"payload").unwrap();
    f.import_dir(&root, &src, "heads/x").unwrap();

    // Drop the handle before tampering: Store keeps hot object caches, so an
    // export in the same process would happily serve the deleted blob from
    // memory and succeed. A cold reopen is what actually exercises the failure.
    drop(f);

    // Remove a reachable blob so the export fails partway through.
    let objects = d.path().join(".forge/objects");
    let mut removed = false;
    for a in fs::read_dir(&objects).unwrap().flatten() {
        for b in fs::read_dir(a.path()).unwrap().flatten() {
            for o in fs::read_dir(b.path()).unwrap().flatten() {
                let bytes = fs::read(o.path()).unwrap_or_default();
                if bytes.windows(7).any(|w| w == b"payload") {
                    fs::remove_file(o.path()).unwrap();
                    removed = true;
                }
            }
        }
    }
    assert!(removed, "test did not find the blob it meant to delete");

    let f = Forge::open(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let tar_path = d.path().join("broken.tar");
    assert!(
        f.export_tar(&root, "heads/x", &tar_path).is_err(),
        "export of a tree with a missing object must fail"
    );
    assert!(
        !tar_path.exists(),
        "a failed export left an artifact behind: a caller that only checks for \
         the file would see a valid-looking empty archive"
    );
}
