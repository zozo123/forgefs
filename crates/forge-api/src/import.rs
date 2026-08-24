//! Lossless host-filesystem import with bounded TOCTOU detection.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::{now_ms, Commit, Tree};
use forge_store::Store;
use forge_types::{Error, ObjectId, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

impl Forge {
    pub fn import_dir(&self, cap: &Cap, dir: &Path, r#ref: &str) -> Result<ObjectId> {
        self.check(cap, Op::Write, Some(r#ref))?;
        let previous = self.store.meta.get_ref(r#ref)?;
        let previous_commit = match previous.as_ref() {
            Some(row) => Some(self.store.get_commit(row.oid)?),
            None => None,
        };
        let tree = import_walk(&self.store, dir, true)?;
        let parents = previous
            .as_ref()
            .map(|row| vec![row.oid])
            .unwrap_or_default();
        let commit = Commit {
            tree,
            parents,
            agent: cap.agent_id().into(),
            msg: format!("import {}", dir.display()),
            ts: now_ms(),
            landmark: false,
            contrib: None,
        };
        let cid = self.store.put_commit(&commit)?;
        let intro_oids = self
            .store
            .collect_intros(previous_commit.as_ref().map(|c| c.tree), tree)?;
        match previous {
            Some(row) => {
                self.store.meta.cas_ref_with_intros(
                    r#ref,
                    row.oid,
                    cid,
                    "commit",
                    cap.agent_id(),
                    cap.agent_id(),
                    false,
                    &intro_oids,
                )?;
            }
            None => {
                self.store.meta.insert_ref_with_intros(
                    r#ref,
                    cid,
                    "commit",
                    false,
                    false,
                    cap.agent_id(),
                    "import",
                    &intro_oids,
                )?;
            }
        }
        Ok(cid)
    }
}

fn import_walk(store: &Store, dir: &Path, source_root: bool) -> Result<ObjectId> {
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

    let reserve = usize::try_from(before.len())
        .unwrap_or(usize::MAX)
        .min(16 * 1024 * 1024);
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
