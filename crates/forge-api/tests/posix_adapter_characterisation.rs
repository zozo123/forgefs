//! Issue #20: what the POSIX adapter does to symlinks, hardlinks, modes,
//! mtimes and sparse files.
//!
//! A ForgeFS TreeEntry is `{name, kind, id, exec}` and FORMAT.md freezes that
//! encoding, so most POSIX metadata has nowhere to live. These tests are the
//! measurement, not an endorsement: each one pins a behaviour that today is
//! silent, so that changing it becomes a deliberate act with a failing test
//! attached rather than an unnoticed drift. `docs/POSIX.md` records which of
//! these are intended semantics and which are open format questions.
#![cfg(unix)]

use forge_api::Forge;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
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

/// (path, entry type, mode, mtime, contents) for every member of the archive.
fn export_members(
    r: &Repo,
    source: &Path,
    name: &str,
) -> Vec<(String, tar::EntryType, u32, u64, Vec<u8>)> {
    r.forge
        .import_dir(&r.cap, source, &format!("heads/{name}"))
        .unwrap();
    let out = source.parent().unwrap().join(format!("{name}.tar"));
    r.forge
        .export_tar(&r.cap, &format!("heads/{name}"), &out)
        .unwrap();
    let mut members = Vec::new();
    let mut archive = tar::Archive::new(fs::File::open(&out).unwrap());
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let header = entry.header();
        let kind = header.entry_type();
        let mode = header.mode().unwrap();
        let mtime = header.mtime().unwrap();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        members.push((path, kind, mode, mtime, data));
    }
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members
}

/// REGRESSION (fixed here): a host I/O failure named no path, so
/// `io: Permission denied (os error 13)` was the entire report for a tree of
/// any size. Import is the adapter at the host boundary; if it refuses because
/// of one file it has to say which file.
#[test]
fn import_io_failure_names_the_offending_path() {
    if unsafe { libc::geteuid() } == 0 {
        // root bypasses the mode check, so the fixture cannot be built.
        return;
    }
    let source = tempdir().unwrap();
    fs::write(source.path().join("readable.txt"), b"ok").unwrap();
    let denied = source.path().join("no-permission.bin");
    fs::write(&denied, b"secret").unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();

    let r = repo();
    let error = r
        .forge
        .import_dir(&r.cap, source.path(), "heads/denied")
        .expect_err("an unreadable source file must fail the import");
    let text = error.to_string();
    assert!(
        text.contains("no-permission.bin"),
        "import error must name the path it could not read, got: {text}"
    );
    assert!(
        text.contains("Permission denied"),
        "import error must keep the underlying errno text, got: {text}"
    );
    // The variant stays Io so the CLI_ABI exit code for a host I/O failure is
    // unchanged; only the message gained its subject.
    assert!(
        matches!(error, forge_types::Error::Io(_)),
        "expected Error::Io, got {error:?}"
    );
}

/// Symlinks are REFUSED, not followed. This is the good news of issue #20:
/// import does not dereference a link and store its target as a regular file,
/// so I2 losslessness is not silently violated. The cost is that no real source
/// tree containing a symlink can be imported at all, and the refusal names one
/// entry per attempt.
#[test]
fn every_symlink_shape_is_refused_by_name() {
    use std::os::unix::fs::symlink;
    let r = repo();
    for (label, build) in [
        ("relative", 0),
        ("absolute", 1),
        ("dangling", 2),
        ("to-directory", 3),
    ] {
        let source = tempdir().unwrap();
        fs::write(source.path().join("real.txt"), b"data").unwrap();
        fs::create_dir(source.path().join("sub")).unwrap();
        let link = source.path().join("the-link");
        match build {
            0 => symlink("real.txt", &link).unwrap(),
            1 => symlink(source.path().join("real.txt"), &link).unwrap(),
            2 => symlink("does-not-exist", &link).unwrap(),
            _ => symlink("sub", &link).unwrap(),
        }
        let error = r
            .forge
            .import_dir(&r.cap, source.path(), "heads/link")
            .expect_err("import must refuse a symlink")
            .to_string();
        assert!(
            error.contains("refuses symlink") && error.contains("the-link"),
            "{label} symlink must be refused by name, got: {error}"
        );
    }
}

/// ASYMMETRY: the refusal is enforced on directory ENTRIES, so a symlink that
/// is itself the import root is silently dereferenced and the import succeeds.
/// `forge import ./link-to-tree` and `forge import ./parent` (which contains
/// `link-to-tree`) therefore disagree about the same link.
#[test]
fn an_import_root_that_is_a_symlink_is_followed_silently() {
    use std::os::unix::fs::symlink;
    let r = repo();
    let holder = tempdir().unwrap();
    let real = holder.path().join("real-tree");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("a.txt"), b"data").unwrap();
    let link = holder.path().join("link-to-tree");
    symlink(&real, &link).unwrap();

    r.forge
        .import_dir(&r.cap, &link, "heads/root-link")
        .expect("an import root that is a symlink is dereferenced, not refused");

    let error = r
        .forge
        .import_dir(&r.cap, holder.path(), "heads/parent")
        .expect_err("the same link one level down is refused")
        .to_string();
    assert!(error.contains("refuses symlink"), "got: {error}");
}

/// SILENT CONVERSION: a hardlinked pair round-trips as two independent regular
/// files. The bytes survive (both names resolve to one deduplicated Blob), the
/// aliasing does not, and nothing reports it.
#[test]
fn hardlinks_flatten_into_independent_regular_files() {
    let source = tempdir().unwrap();
    let a = source.path().join("a.txt");
    fs::write(&a, b"shared").unwrap();
    fs::hard_link(&a, source.path().join("b.txt")).unwrap();
    assert_eq!(
        fs::metadata(&a).unwrap().nlink(),
        2,
        "fixture must really be a hardlink"
    );

    let r = repo();
    let members = export_members(&r, source.path(), "hardlink");
    assert_eq!(members.len(), 2, "{members:?}");
    for (path, kind, _, _, data) in &members {
        assert_eq!(
            *kind,
            tar::EntryType::Regular,
            "{path} exported as {kind:?}, not as a tar hardlink member"
        );
        assert_eq!(data, b"shared");
    }
}

use std::os::unix::fs::MetadataExt;

/// SILENT CONVERSION: every permission bit except "any execute bit is set"
/// is discarded at import, and export then invents 0644/0755. The direction
/// matters: 0600 becomes 0644, so a mode that was owner-only on the way in is
/// world-readable on the way out. setuid/setgid are dropped, which is the safe
/// direction; the read/write widening is not.
#[test]
fn permission_bits_collapse_to_one_exec_bit_and_export_widens_them() {
    let source = tempdir().unwrap();
    let cases: &[(&str, u32, u32)] = &[
        // (name, source mode, mode the exported archive asks for)
        ("secret.txt", 0o600, 0o644),
        ("readonly.txt", 0o444, 0o644),
        ("odd.txt", 0o741, 0o755),
        ("exec.sh", 0o755, 0o755),
        ("setuid.bin", 0o4755, 0o755),
        ("setgid.bin", 0o2755, 0o755),
    ];
    for (name, mode, _) in cases {
        let p = source.path().join(name);
        fs::write(&p, b"x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
    }

    let r = repo();
    let members = export_members(&r, source.path(), "modes");
    for (name, _, want) in cases {
        let got = members
            .iter()
            .find(|m| m.0 == *name)
            .unwrap_or_else(|| panic!("{name} missing from {members:?}"))
            .2;
        assert_eq!(
            got, *want,
            "{name}: exported mode {got:o}, expected {want:o}"
        );
    }
    let secret = members.iter().find(|m| m.0 == "secret.txt").unwrap().2;
    assert_ne!(
        secret & 0o077,
        0,
        "0600 must be recorded as widening to a group/other-readable mode; \
         if this now fails the adapter gained a real mode and docs/POSIX.md \
         needs rewriting"
    );
}

/// SILENT CONVERSION: mtime has nowhere to live in a v1 Tree, so export stamps
/// the epoch on every member. That is deterministic (two exports of one tree
/// are byte-identical), which is why it is a defensible choice, but it is still
/// unrecoverable metadata loss and nothing says so.
#[test]
fn mtimes_are_replaced_by_the_epoch() {
    let source = tempdir().unwrap();
    fs::create_dir(source.path().join("d")).unwrap();
    fs::write(source.path().join("d/old.txt"), b"t").unwrap();
    let before = fs::metadata(source.path().join("d/old.txt"))
        .unwrap()
        .mtime();
    assert!(before > 0, "fixture must have a non-zero mtime");

    let r = repo();
    for (path, _, _, mtime, _) in export_members(&r, source.path(), "mtime") {
        assert_eq!(mtime, 0, "{path} exported with mtime {mtime}, expected 0");
    }
}

/// SILENT CONVERSION with a resource cost: import reads holes as zero bytes, so
/// a sparse file is materialised at its APPARENT length in the object store, in
/// the archive, and again on extraction. Measured at 100 MiB apparent / 0
/// allocated blocks this turned a 4 KiB source directory into ~101 MiB of
/// objects and a ~101 MiB archive.
#[test]
fn sparse_holes_are_materialised_at_apparent_length() {
    const LEN: u64 = 1 << 20;
    let source = tempdir().unwrap();
    let p = source.path().join("sparse.bin");
    let f = fs::File::create(&p).unwrap();
    f.set_len(LEN).unwrap();
    drop(f);
    let meta = fs::metadata(&p).unwrap();
    assert_eq!(meta.len(), LEN);
    let sparse_fixture = meta.blocks() * 512 < LEN;

    let r = repo();
    let members = export_members(&r, source.path(), "sparse");
    assert_eq!(members.len(), 1);
    let (_, kind, _, _, data) = &members[0];
    assert_eq!(*kind, tar::EntryType::Regular, "not a tar sparse member");
    assert_eq!(
        data.len() as u64,
        LEN,
        "the hole is stored as {LEN} literal zero bytes"
    );
    assert!(data.iter().all(|b| *b == 0));
    if sparse_fixture {
        // Only assert the amplification when the host filesystem actually gave
        // us a hole; tmpfs/overlayfs variants that materialise it up front are
        // not evidence about ForgeFS.
        assert!(
            meta.blocks() * 512 < data.len() as u64,
            "source allocated {} bytes, ForgeFS stored {}",
            meta.blocks() * 512,
            data.len()
        );
    }
}

/// A `.git` or `.forge` directory at the import ROOT is dropped without a word
/// and the import reports success, so the exported tree is not the tree that
/// was handed to import. The same name nested one level down is user data and
/// is preserved. This is deliberate (see import.rs) but it is the one remaining
/// place where import silently returns less than it was given.
#[test]
fn a_root_control_directory_is_dropped_without_a_diagnostic() {
    let source = tempdir().unwrap();
    fs::create_dir_all(source.path().join(".git")).unwrap();
    fs::write(source.path().join(".git/config"), b"top").unwrap();
    fs::create_dir_all(source.path().join("sub/.git")).unwrap();
    fs::write(source.path().join("sub/.git/config"), b"nested").unwrap();
    fs::write(source.path().join("a.txt"), b"a").unwrap();

    let r = repo();
    let names: Vec<String> = export_members(&r, source.path(), "control")
        .into_iter()
        .map(|m| m.0)
        .collect();
    assert!(
        !names.iter().any(|n| n == ".git/config"),
        "root .git was expected to be dropped, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "sub/.git/config"),
        "nested .git is user data and must survive, got {names:?}"
    );
}
