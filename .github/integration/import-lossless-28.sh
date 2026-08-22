#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main
git merge --no-edit origin/main

python3 - <<'PY'
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if text.count(old) != 1:
        raise SystemExit(f"{label}: expected one marker, found {text.count(old)}")
    return text.replace(old, new, 1)

p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    "use std::io::Write;\n",
    "use std::io::{Read, Seek, SeekFrom, Write};\n",
    "io imports",
)

old_walk = '''fn import_walk(store: &Store, dir: &Path, source_root: bool) -> Result<ObjectId> {
    let mut entries = Vec::new();
    // Never turn a per-entry enumeration error into a successful partial import.
    let mut kids: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    kids.sort_by_key(|e| e.file_name());
    for k in kids {
        let name = k
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        // Root control directories are outside the import domain. Nested names
        // with the same spelling are ordinary user data and must be preserved.
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_symlink() {
            return Err(Error::Invalid(format!(
                "import refuses symlink {}",
                k.path().display()
            )));
        }
        if ft.is_dir() {
            let id = import_walk(store, &k.path(), false)?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "import refuses unsupported file type {}",
                k.path().display()
            )));
        } else {
            let data = fs::read(k.path())?;
            let id = store.put_blob_data(&data)?;
            let exec = is_exec(&k.path())?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Blob,
                id,
                exec,
            });
        }
    }
    store.put_tree(&Tree::new(entries)?)
}
'''
new_walk = '''fn import_walk(store: &Store, dir: &Path, source_root: bool) -> Result<ObjectId> {
    let mut entries = Vec::new();
    // Never turn a per-entry enumeration error into a successful partial import.
    // Snapshot the in-scope directory membership and require it to be unchanged
    // after all children are processed; additions/deletions/renames are a failed
    // import rather than an allegedly exact partial snapshot.
    let kids = import_dir_entries(dir)?;
    let expected_names = import_scoped_names(&kids, dir, source_root)?;
    for k in kids {
        let name = k
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        // Root control directories are outside the import domain. Nested names
        // with the same spelling are ordinary user data and must be preserved.
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_symlink() {
            return Err(Error::Invalid(format!(
                "import refuses symlink {}",
                k.path().display()
            )));
        }
        if ft.is_dir() {
            let id = import_walk(store, &k.path(), false)?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "import refuses unsupported file type {}",
                k.path().display()
            )));
        } else {
            let (data, exec) = read_import_file(&k.path())?;
            let id = store.put_blob_data(&data)?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Blob,
                id,
                exec,
            });
        }
    }
    let observed_names = import_scoped_names(&import_dir_entries(dir)?, dir, source_root)?;
    if observed_names != expected_names {
        return Err(Error::Invalid(format!(
            "source directory changed during import: {}",
            dir.display()
        )));
    }
    store.put_tree(&Tree::new(entries)?)
}

fn import_dir_entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut kids: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    kids.sort_by_key(|e| e.file_name());
    Ok(kids)
}

fn import_scoped_names(
    kids: &[fs::DirEntry],
    dir: &Path,
    source_root: bool,
) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(kids.len());
    for kid in kids {
        let name = kid
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        names.push(name);
    }
    Ok(names)
}

fn read_import_file(path: &Path) -> Result<(Vec<u8>, bool)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let before = file.metadata()?;
    if !before.file_type().is_file() {
        return Err(Error::Invalid(format!(
            "import refuses non-regular file {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    let exec = {
        use std::os::unix::fs::PermissionsExt;
        before.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let exec = false;

    let reserve = usize::try_from(before.len()).unwrap_or(usize::MAX).min(16 * 1024 * 1024);
    let mut data = Vec::with_capacity(reserve);
    file.read_to_end(&mut data)?;

    // A second read from the same descriptor catches content mutation even on
    // filesystems with coarse timestamp granularity, without allocating a second
    // full-file buffer.
    file.seek(SeekFrom::Start(0))?;
    let mut offset = 0usize;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let end = offset.saturating_add(n);
        if end > data.len() || data[offset..end] != buf[..n] {
            return Err(Error::Invalid(format!(
                "source file changed during import: {}",
                path.display()
            )));
        }
        offset = end;
    }
    if offset != data.len() {
        return Err(Error::Invalid(format!(
            "source file changed during import: {}",
            path.display()
        )));
    }

    let after = file.metadata()?;
    if !import_file_metadata_stable(&before, &after) {
        return Err(Error::Invalid(format!(
            "source file metadata changed during import: {}",
            path.display()
        )));
    }

    // The pathname must still name the same regular file we opened. This closes
    // the common rename/symlink-swap TOCTOU without pretending to provide a host
    // filesystem snapshot primitive.
    let path_after = fs::symlink_metadata(path)?;
    if !path_after.file_type().is_file() {
        return Err(Error::Invalid(format!(
            "source path changed type during import: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    if path_after.dev() != after.dev() || path_after.ino() != after.ino() {
        return Err(Error::Invalid(format!(
            "source path changed identity during import: {}",
            path.display()
        )));
    }
    #[cfg(not(unix))]
    if path_after.len() != after.len() || path_after.modified().ok() != after.modified().ok() {
        return Err(Error::Invalid(format!(
            "source path changed identity during import: {}",
            path.display()
        )));
    }

    Ok((data, exec))
}

fn import_file_metadata_stable(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && before.permissions().mode() == after.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && before.permissions().readonly() == after.permissions().readonly()
    }
}
'''
s = replace_once(s, old_walk, new_walk, "import walk")
p.write_text(s)

# Add a process-independent matrix of source representations the v1 model
# cannot faithfully encode. Success must never mean silent loss.
t = Path("crates/forge-api/tests/import_lossless.rs")
t.write_text(r'''use forge_api::Forge;
use tempfile::tempdir;
use std::fs;

fn import_fails(source: &std::path::Path) {
    let dst = tempdir().unwrap();
    let forge = Forge::init(dst.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    assert!(
        forge.import_dir(&cap, source, "heads/import").is_err(),
        "unsupported or unstable source must fail closed"
    );
}

#[cfg(unix)]
#[test]
fn import_rejects_symlink_fifo_socket_and_non_utf8_name() {
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    {
        let source = tempdir().unwrap();
        fs::write(source.path().join("target"), b"data").unwrap();
        symlink("target", source.path().join("link")).unwrap();
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let fifo = source.path().join("pipe");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let _listener = UnixListener::bind(source.path().join("sock")).unwrap();
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let bad = OsString::from_vec(b"bad-\xff".to_vec());
        fs::write(source.path().join(bad), b"data").unwrap();
        import_fails(source.path());
    }
}

#[cfg(unix)]
#[test]
fn import_rejects_a_regular_file_mutating_during_snapshot() {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };
    use std::thread;
    use std::time::Duration;

    let source = tempdir().unwrap();
    let path = source.path().join("moving.bin");
    fs::write(&path, vec![0u8; 32 * 1024 * 1024]).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let writer_path = path.clone();
    let writer_stop = Arc::clone(&stop);
    let writer_barrier = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        let mut file = fs::OpenOptions::new().write(true).open(writer_path).unwrap();
        let mut byte = 1u8;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte]).unwrap();
        writer_barrier.wait();
        while !writer_stop.load(Ordering::Relaxed) {
            byte = byte.wrapping_add(1);
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[byte]).unwrap();
            std::hint::spin_loop();
        }
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(10));
    let dst = tempdir().unwrap();
    let forge = Forge::init(dst.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    let result = forge.import_dir(&cap, source.path(), "heads/import");
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(result.is_err(), "a changing source file must never import successfully");
}
''')
PY

cargo fmt --all
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked

git rm -f .github/trigger-import-lossless-28 2>/dev/null || true
git add crates/forge-api/src/lib.rs crates/forge-api/tests/import_lossless.rs
git commit -m 'fix: make import source snapshot fail closed (#28)'
git push origin HEAD:fix/import-lossless-28
