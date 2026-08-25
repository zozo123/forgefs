//! Issue #20: symlink handling at the import boundary.
//!
//! A VERSION 1 `TreeEntry` is `{name, oid, kind, exec}` with `kind` in
//! `{Blob, Tree}` and FORMAT.md freezes that encoding, so there is nowhere to
//! record "this entry is a link". Import therefore has two honest behaviours,
//! and these tests pin both:
//!
//! * default -- refuse, naming EVERY symlink in the tree in one pass;
//! * `--follow-symlinks` -- materialise the target's CONTENT, but only when the
//!   target resolves INSIDE the import root.
//!
//! The containment rule is the load-bearing one. Without it, following a link
//! copies bytes from outside the import root into the object store, which is a
//! path escape driven entirely by the contents of an untrusted source tree.
#![cfg(unix)]

use forge_api::{Forge, ImportOptions};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use tempfile::{tempdir, TempDir};

struct Repo {
    forge: Forge,
    cap: forge_cap::Cap,
    _dir: TempDir,
}

fn repo() -> Repo {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    Repo {
        forge,
        cap,
        _dir: dir,
    }
}

const FOLLOW: ImportOptions = ImportOptions {
    follow_symlinks: true,
};

fn import(r: &Repo, source: &Path, name: &str, options: ImportOptions) -> forge_types::Result<()> {
    r.forge
        .import_dir_with(&r.cap, source, &format!("heads/{name}"), options)
        .map(|_| ())
}

/// Every byte of every blob the repository can reach from `ref`.
fn exported_bytes(r: &Repo, name: &str) -> Vec<u8> {
    let out = r._dir.path().join(format!("{name}.tar"));
    r.forge
        .export_tar(&r.cap, &format!("heads/{name}"), &out)
        .unwrap();
    fs::read(&out).unwrap()
}

/// (label, fixture builder) for a table-driven case.
type Case = (&'static str, &'static dyn Fn(&Path));

fn ref_exists(r: &Repo, name: &str) -> bool {
    r.forge
        .refs(&r.cap)
        .unwrap()
        .iter()
        .any(|row| row.name == format!("heads/{name}"))
}

/// THE SECURITY PROPERTY. `--follow-symlinks` is an instruction to materialise
/// link targets, NOT an instruction to leave the import root. A source tree is
/// untrusted input: if a link in it can name `/etc/passwd` or `../../` and have
/// those bytes copied into the object store, then importing any third-party
/// repository exfiltrates host files into a content-addressed, shareable,
/// sealable store. Each case below is refused by name and nothing is published.
#[test]
fn follow_symlinks_never_lets_bytes_from_outside_the_root_into_the_store() {
    let holder = tempdir().unwrap();
    let outside = holder.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"SECRET-OUTSIDE-THE-ROOT").unwrap();

    // A sibling whose path shares a textual prefix with the root must not count
    // as "inside": containment is component-wise, not `str::starts_with`.
    let prefix_sibling = holder.path().join("root-evil");
    fs::create_dir(&prefix_sibling).unwrap();
    fs::write(prefix_sibling.join("evil.txt"), b"SECRET-OUTSIDE-THE-ROOT").unwrap();

    let cases: &[Case] = &[
        ("absolute", &|root: &Path| {
            symlink("/etc/passwd", root.join("escape")).unwrap()
        }),
        ("relative-dir", &|root: &Path| {
            symlink("../outside", root.join("escape")).unwrap()
        }),
        ("relative-file", &|root: &Path| {
            symlink("../outside/secret.txt", root.join("escape")).unwrap()
        }),
        ("prefix-sibling", &|root: &Path| {
            symlink("../root-evil", root.join("escape")).unwrap()
        }),
        ("via-inner-dir", &|root: &Path| {
            fs::create_dir(root.join("inner")).unwrap();
            symlink("../../outside", root.join("inner/escape")).unwrap()
        }),
        ("chained", &|root: &Path| {
            // hop through a link that IS inside the root before leaving it
            symlink("../outside/secret.txt", root.join("hop")).unwrap();
            symlink("hop", root.join("escape")).unwrap();
        }),
    ];

    for (label, build) in cases {
        let root = holder.path().join("root");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("legit.txt"), b"legit").unwrap();
        build(&root);

        let r = repo();
        let error = import(&r, &root, label, FOLLOW)
            .expect_err("a symlink target outside the import root must be refused")
            .to_string();
        assert!(
            error.contains("escape") || error.contains("hop"),
            "{label}: refusal must name the offending link, got: {error}"
        );
        assert!(
            error.contains("outside the import root"),
            "{label}: refusal must say the target escaped the root, got: {error}"
        );
        assert!(
            !ref_exists(&r, label),
            "{label}: a refused import must publish no ref"
        );
        // Belt and braces: even if a ref had been published, the bytes from
        // outside must never have been stored under it.
        if ref_exists(&r, label) {
            let bytes = exported_bytes(&r, label);
            assert!(
                !bytes.windows(23).any(|w| w == b"SECRET-OUTSIDE-THE-ROOT"),
                "{label}: bytes from outside the import root entered the repository"
            );
        }
    }
}

/// Contained links are what `--follow-symlinks` is FOR: a file link becomes a
/// regular blob under the link's name, a directory link becomes a tree.
#[test]
fn follow_symlinks_materialises_contained_targets() {
    let source = tempdir().unwrap();
    fs::write(source.path().join("real.txt"), b"real-content").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/inner.txt"), b"inner").unwrap();
    symlink("real.txt", source.path().join("filelink")).unwrap();
    symlink("sub", source.path().join("dirlink")).unwrap();

    let r = repo();
    import(&r, source.path(), "follow", FOLLOW).expect("contained links must import");
    let tar = exported_bytes(&r, "follow");
    let mut names: Vec<String> = tar::Archive::new(&tar[..])
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "dirlink/",
            "dirlink/inner.txt",
            "filelink",
            "real.txt",
            "sub/",
            "sub/inner.txt",
        ],
        "link names must survive as ordinary entries carrying the target's content"
    );
}

/// A dangling link and every loop shape terminate with exit-1 input errors.
/// They must never hang, recurse without bound, or reach the internal-failure
/// exit code: all of them are caller-supplied source-tree content.
#[test]
fn follow_symlinks_refuses_dangling_links_and_every_loop_shape() {
    let cases: &[Case] = &[
        ("dangling", &|root: &Path| {
            symlink("does-not-exist", root.join("l")).unwrap()
        }),
        ("two-link-cycle", &|root: &Path| {
            symlink("bb", root.join("aa")).unwrap();
            symlink("aa", root.join("bb")).unwrap();
        }),
        ("self-directory", &|root: &Path| {
            symlink(".", root.join("selfloop")).unwrap()
        }),
        ("mutual-directories", &|root: &Path| {
            fs::create_dir(root.join("x")).unwrap();
            fs::create_dir(root.join("y")).unwrap();
            symlink("../y", root.join("x/toy")).unwrap();
            symlink("../x", root.join("y/tox")).unwrap();
        }),
    ];
    for (label, build) in cases {
        let source = tempdir().unwrap();
        fs::write(source.path().join("real.txt"), b"real").unwrap();
        build(source.path());

        let r = repo();
        let error = import(&r, source.path(), label, FOLLOW)
            .expect_err("a dangling link or a loop must be refused, not followed");
        assert!(
            matches!(error, forge_types::Error::Invalid(_)),
            "{label}: source-tree content must map to the input exit code, got {error:?}"
        );
        assert!(
            error.to_string().contains("refuses symlink"),
            "{label}: got {error}"
        );
        assert!(!ref_exists(&r, label), "{label}: nothing may be published");
    }
}

/// The default is still refusal, but one run now reports the whole job. Failing
/// on the first symlink turned preparing a real repository into a
/// fix-one-rerun loop, which is the practical half of the adoption blocker.
#[test]
fn the_default_refusal_names_every_symlink_in_one_pass() {
    let source = tempdir().unwrap();
    fs::write(source.path().join("real.txt"), b"data").unwrap();
    fs::create_dir_all(source.path().join("deep/deeper")).unwrap();
    symlink("real.txt", source.path().join("aaa-link")).unwrap();
    symlink("real.txt", source.path().join("mmm-link")).unwrap();
    symlink("/etc/passwd", source.path().join("deep/zzz-link")).unwrap();
    symlink("nowhere", source.path().join("deep/deeper/nested-link")).unwrap();

    let r = repo();
    let error = import(&r, source.path(), "many", ImportOptions::default())
        .expect_err("the default must still refuse")
        .to_string();
    for name in ["aaa-link", "mmm-link", "zzz-link", "nested-link"] {
        assert!(
            error.contains(name),
            "one refusal must name every symlink; {name} missing from: {error}"
        );
    }
    assert!(
        error.contains("--follow-symlinks"),
        "the refusal must name the opt-in that makes the import possible, got: {error}"
    );
}

/// Closes the root asymmetry recorded in docs/POSIX.md: importing a symlink to
/// a tree and importing the tree itself now agree, because the root is resolved
/// to its real path before the walk and that real path is the containment root.
/// Commit oids embed a timestamp, so the comparison is on exported content.
#[test]
fn a_symlinked_import_root_agrees_with_importing_its_target() {
    let holder = tempdir().unwrap();
    let real = holder.path().join("real-tree");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("a.txt"), b"data").unwrap();
    fs::create_dir(real.join("d")).unwrap();
    fs::write(real.join("d/b.txt"), b"more").unwrap();
    let link = holder.path().join("link-to-tree");
    symlink(&real, &link).unwrap();

    let r = repo();
    import(&r, &real, "direct", ImportOptions::default()).unwrap();
    import(&r, &link, "vialink", ImportOptions::default()).unwrap();
    assert_eq!(
        exported_bytes(&r, "direct"),
        exported_bytes(&r, "vialink"),
        "importing a symlinked root must produce the same content as importing its target"
    );
}

/// A symlinked root does not widen containment: the root's REAL path is what
/// links are checked against, so a link inside it still cannot climb out.
#[test]
fn a_symlinked_import_root_does_not_widen_containment() {
    let holder = tempdir().unwrap();
    let real = holder.path().join("real-tree");
    fs::create_dir(&real).unwrap();
    fs::write(
        holder.path().join("sibling.txt"),
        b"SECRET-OUTSIDE-THE-ROOT",
    )
    .unwrap();
    symlink("../sibling.txt", real.join("escape")).unwrap();
    let link = holder.path().join("link-to-tree");
    symlink(&real, &link).unwrap();

    let r = repo();
    let error = import(&r, &link, "vialink", FOLLOW)
        .expect_err("a link inside a symlinked root must still be contained")
        .to_string();
    assert!(error.contains("outside the import root"), "got: {error}");
}
