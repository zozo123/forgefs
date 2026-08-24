//! I16: tree names are exact UTF-8 bytes, so `Foo` and `foo`, and the NFC and
//! NFD spellings of one name, are DISTINCT ForgeFS entries. INVARIANTS.md adds
//! the adapter half: "Export adapters must detect target-filesystem collisions
//! and fail rather than silently normalize, fold, or overwrite names."
//!
//! Export used to write such a tree happily. The tar itself is faithful -- the
//! loss lands on whoever extracts it, because on a case-insensitive or
//! normalizing filesystem (APFS, NTFS) the second member overwrites the first
//! and the extracted tree quietly holds one file where the repository held two.
//! Nothing about the archive reveals it afterwards.
//!
//! The collision is a property of the TREE, not of the machine running the
//! test, so these run everywhere: the trees are built through the API rather
//! than on disk, which also keeps the fixtures honest on a case-insensitive
//! developer machine where the on-disk fixture could not exist at all.

use forge_api::{ExportOptions, Forge};
use forge_cap::Cap;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

/// Commit `names` onto `work`, byte for byte. Built through the API, not on
/// disk: the fixture must exist even on a case-insensitive developer machine,
/// where `Foo` and `foo` could not both be created.
fn commit_names(f: &Forge, cap: &Cap, names: &[&str]) {
    f.branch(cap, "main", "work").unwrap();
    let ns = f.session_open(cap, "work").unwrap();
    f.mount(cap, &ns, "/", "ref:work", true).unwrap();
    for n in names {
        f.write(cap, &ns, &format!("/{n}"), n.as_bytes(), false)
            .unwrap();
    }
    f.checkin(cap, &ns, "/", "seed").unwrap();
}

fn export_error(f: &Forge, cap: &Cap, ref_name: &str, out: &Path) -> String {
    match f.export_tar(cap, ref_name, out) {
        Ok(()) => panic!(
            "export wrote {} for a tree whose sibling names collide on a \
             case-insensitive or normalizing filesystem: extraction there loses one \
             of them silently, which is exactly what I16 tells export adapters to \
             detect and refuse",
            out.display()
        ),
        Err(e) => e.to_string(),
    }
}

/// Both names must appear, and they must be distinguishable: an error that
/// prints two visually identical names is not an error an operator can act on.
fn assert_names_reported(msg: &str, a: &str, b: &str) {
    for name in [a, b] {
        assert!(
            msg.contains(&format!("{name:?}")),
            "error must name the colliding entry {name:?}: {msg}"
        );
        let hex: Vec<String> = name.bytes().map(|x| format!("{x:02x}")).collect();
        assert!(
            msg.contains(&hex.join(" ")),
            "error must show the exact bytes of {name:?} so two spellings that render \
             identically can be told apart: {msg}"
        );
    }
}

#[test]
fn i16_export_refuses_ascii_case_collisions() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    commit_names(&f, &root, &["Foo", "foo", "unrelated.txt"]);

    let out = d.path().join("case.tar");
    let msg = export_error(&f, &root, "work", &out);
    assert_names_reported(&msg, "Foo", "foo");
    assert!(
        !out.exists(),
        "a refused export must leave no artifact behind: {}",
        out.display()
    );
    assert_eq!(
        fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("case.tar"))
            .count(),
        0,
        "a refused export left a partial file beside the destination"
    );
}

#[test]
fn i16_export_refuses_nfc_nfd_collisions() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    let nfc = "caf\u{e9}.txt";
    let nfd = "cafe\u{301}.txt";
    commit_names(&f, &root, &[nfc, nfd]);

    let out = d.path().join("norm.tar");
    let msg = export_error(&f, &root, "work", &out);
    assert_names_reported(&msg, nfc, nfd);
    assert!(!out.exists(), "a refused export must leave no artifact");
}

#[test]
fn i16_export_refuses_collisions_below_the_root() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    commit_names(&f, &root, &["dir/Bar", "dir/bar", "top.txt"]);

    let out = d.path().join("deep.tar");
    let msg = export_error(&f, &root, "work", &out);
    assert_names_reported(&msg, "Bar", "bar");
    assert!(
        msg.contains("dir"),
        "error must say which directory holds the collision: {msg}"
    );
}

/// The opt-out is deliberate, per call, and never inferred from the host.
#[test]
fn i16_collisions_are_exportable_only_by_explicit_opt_out() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    commit_names(&f, &root, &["Foo", "foo"]);

    let out = d.path().join("opt.tar");
    f.export_tar_with(
        &root,
        "work",
        &out,
        ExportOptions {
            allow_name_collisions: true,
        },
    )
    .expect("the opt-out must still produce the archive: a tar is not itself case-insensitive");

    let listed = Command::new("tar").arg("-tf").arg(&out).output().unwrap();
    assert!(listed.status.success(), "tar -tf failed: {listed:?}");
    let listing = String::from_utf8_lossy(&listed.stdout);
    let members: Vec<&str> = listing.lines().filter(|l| l.ends_with("oo")).collect();
    assert_eq!(
        members.len(),
        2,
        "both spellings must survive into the archive: {listing:?}"
    );
}

/// The detector must not fire on names that merely look related. Failing a
/// legitimate export is its own data-availability bug.
#[test]
fn i16_distinct_but_non_colliding_names_still_export() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();
    commit_names(
        &f,
        &root,
        &[
            "resume.txt",
            "r\u{e9}sum\u{e9}.txt",
            "Alpha.txt",
            "beta.txt",
            "dir/Alpha.txt",
        ],
    );

    let out = d.path().join("clean.tar");
    f.export_tar(&root, "work", &out)
        .expect("no two of these names collide under case folding or NFC/NFD");
    assert!(out.exists());
}

/// I2 still holds for the ordinary case: detection must not disturb the
/// export/extract/re-import round trip.
#[test]
fn i16_detection_does_not_break_the_lossless_round_trip() {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let root = f.root_cap().unwrap();

    let src = d.path().join("src");
    fs::create_dir_all(src.join("Nested")).unwrap();
    fs::write(src.join("Alpha.txt"), b"a").unwrap();
    fs::write(src.join("beta.txt"), b"b").unwrap();
    fs::write(src.join("Nested/Deep.txt"), b"d").unwrap();
    f.import_dir(&root, &src, "heads/rt").unwrap();

    let out = d.path().join("rt.tar");
    f.export_tar(&root, "heads/rt", &out).unwrap();
    let back = d.path().join("back");
    fs::create_dir_all(&back).unwrap();
    let extracted = Command::new("tar")
        .arg("-xf")
        .arg(&out)
        .arg("-C")
        .arg(&back)
        .output()
        .unwrap();
    assert!(extracted.status.success(), "tar -xf failed: {extracted:?}");
    f.import_dir(&root, &back, "heads/rt-back").unwrap();

    // Commits embed a timestamp, so compare content-addressed listings.
    let before = listing_of(&f, &root, "heads/rt");
    let after = listing_of(&f, &root, "heads/rt-back");
    assert_eq!(before, after, "collision detection broke the round trip");
    assert!(before.len() >= 4, "listing too small to prove anything");
}

/// Every entry reachable from a ref, as (path, kind, oid, exec), sorted.
fn listing_of(f: &Forge, cap: &Cap, ref_name: &str) -> Vec<(String, String, String, bool)> {
    let ns = f.session_open(cap, ref_name).unwrap();
    f.mount(cap, &ns, "/", &format!("ref:{ref_name}"), false)
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
