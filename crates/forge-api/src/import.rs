//! Lossless host-filesystem import with bounded TOCTOU detection.

use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::{now_ms, Commit, Tree, MAX_TREE_ENTRIES};
use forge_store::Store;
use forge_types::{CasResult, Error, ObjectId, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// How `import_dir_with` treats host entries that a VERSION 1 Tree cannot
/// represent.
///
/// A v1 `TreeEntry` is `{name, oid, kind, exec}` with `kind` in `{Blob, Tree}`.
/// There is no encoding for a symlink and FORMAT.md freezes that, so import has
/// exactly two honest behaviours: refuse, or materialise the target's content
/// and say so. The default is refuse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Replace each symlink with the CONTENT of its target: a file link becomes
    /// a regular Blob under the LINK's name, a directory link becomes a Tree.
    ///
    /// This is lossy and deliberately opt-in. It is also contained: a target
    /// that resolves outside the import root is refused, so no bytes from
    /// outside the root can enter the repository. Dangling links and symlink
    /// loops are refused rather than followed.
    pub follow_symlinks: bool,
}

/// Cap on the number of symlink paths named in one refusal diagnostic.
const SYMLINK_REPORT_LIMIT: usize = 32;
/// Backstop on directory nesting while following links; the (dev, ino) stack is
/// what actually detects loops, this only bounds pathological real trees.
const MAX_IMPORT_DEPTH: usize = 256;

impl Forge {
    /// Import an exact directory snapshot and return the ref publication outcome.
    /// A lost compare-and-swap is preserved and reported as an explicit fork.
    pub fn import_dir(&self, cap: &Cap, dir: &Path, r#ref: &str) -> Result<CasResult> {
        self.import_dir_with(cap, dir, r#ref, ImportOptions::default())
    }

    /// Import an exact directory snapshot under explicit host-adapter options.
    pub fn import_dir_with(
        &self,
        cap: &Cap,
        dir: &Path,
        r#ref: &str,
        options: ImportOptions,
    ) -> Result<CasResult> {
        self.check(cap, Op::Write, Some(r#ref))?;
        let previous = self.store.meta.get_ref(r#ref)?;
        let previous_commit = match previous.as_ref() {
            Some(row) => Some(self.store.get_commit(row.oid)?),
            None => None,
        };
        let expected = previous
            .as_ref()
            .map(|row| row.oid)
            .unwrap_or(ObjectId::ZERO);
        crate::test_hooks::process_barrier(
            "FORGEFS_TEST_IMPORT_SNAPSHOT_BARRIER",
            2,
            "import snapshot",
        )?;
        // Resolving the root first is what makes containment definable: every
        // followed link target is checked against THIS path, and an import root
        // that is itself a symlink behaves exactly like importing its target.
        let root_real = fs::canonicalize(dir).map_err(|e| io_at(dir, e))?;
        let mut walk = ImportWalk {
            store: &self.store,
            root_real: root_real.clone(),
            options,
            stack: Vec::new(),
        };
        let tree = match walk.walk(&root_real, true) {
            Ok(tree) => tree,
            // A symlink refusal is an adoption problem, not a mystery: sweep the
            // tree once (lstat only, on the already-failing path) so one run
            // reports every offending path instead of a fix-one-rerun loop.
            // Only the default refusal is rewritten. Under --follow-symlinks a
            // symlink error is specific ("outside the import root", "target does
            // not exist", "symlink loop") and that reason is the whole message.
            Err(Error::Invalid(msg))
                if !options.follow_symlinks && msg.starts_with(SYMLINK_REFUSAL_PREFIX) =>
            {
                return Err(Error::Invalid(symlink_refusal(&root_real, &msg)));
            }
            Err(e) => return Err(e),
        };
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
        self.store.meta.cas_ref_with_intros(
            r#ref,
            expected,
            cid,
            "commit",
            cap.agent_id(),
            cap.agent_id(),
            false,
            &intro_oids,
        )
    }
}

pub(crate) const SYMLINK_REFUSAL_PREFIX: &str = "import refuses symlink ";

struct ImportWalk<'a> {
    store: &'a Store,
    /// Canonical import root. Every followed link target must live under it.
    root_real: PathBuf,
    options: ImportOptions,
    /// (dev, ino) of every directory on the current recursion path. A followed
    /// directory symlink that re-enters one of them is a loop, not a tree, and
    /// this is what makes `a -> b -> a` terminate instead of recursing forever.
    stack: Vec<(u64, u64)>,
}

impl ImportWalk<'_> {
    fn walk(&mut self, dir: &Path, source_root: bool) -> Result<ObjectId> {
        if self.stack.len() >= MAX_IMPORT_DEPTH {
            return Err(Error::Invalid(format!(
                "import refuses source deeper than {MAX_IMPORT_DEPTH} directories: {}",
                dir.display()
            )));
        }
        // Hold the directory open O_NOFOLLOW for the whole enumeration and pin
        // its identity. `fs::read_dir` resolves a PATH, so without this a
        // directory swapped for a symlink between the dirent read and the
        // descent would be walked and content from outside the root could enter
        // the tree. The open refuses the swap outright; the identity re-check
        // after enumeration catches a swap that races the open.
        let pinned = open_directory_nofollow(dir)?;
        let mut entries = Vec::new();
        // Never turn a per-entry enumeration error into a successful partial import.
        // Snapshot the in-scope directory membership and require it to be unchanged
        // after all children are processed; additions/deletions/renames are a failed
        // import rather than an allegedly exact partial snapshot.
        let kids = import_dir_entries(dir)?;
        let expected_names = import_scoped_names(&kids, dir, source_root)?;
        // #355: a directory with more entries than a VERSION 1 tree can hold is
        // the caller passing us something too big, not a damaged repository, so
        // it is `Invalid` (exit 1) and the refusal names the directory, its
        // size and the limit. It used to be discovered on the way back OUT of
        // the store, as `Corrupt` -- exit 2, "this repository is damaged" -- for
        // an untouched repository and an intact source directory.
        //
        // Checked here, on the dirents, rather than on the assembled entries:
        // this is before a single blob of the doomed directory is read or put,
        // so the refusal costs one readdir instead of a full walk.
        if expected_names.len() as u64 > MAX_TREE_ENTRIES {
            return Err(Error::Invalid(format!(
                "import refuses {}: it holds {} entries, more than the {MAX_TREE_ENTRIES} a \
                 tree may hold; split it into subdirectories",
                dir.display(),
                expected_names.len()
            )));
        }
        if let Some(id) = pinned {
            self.stack.push(id);
        }
        let result = self.walk_children(dir, source_root, kids, &mut entries);
        if pinned.is_some() {
            self.stack.pop();
        }
        result?;
        let observed_names = import_scoped_names(&import_dir_entries(dir)?, dir, source_root)?;
        if observed_names != expected_names {
            return Err(Error::Invalid(format!(
                "source directory changed during import: {}",
                dir.display()
            )));
        }
        verify_directory_identity(dir, pinned)?;
        self.store.put_tree(&Tree::new(entries)?)
    }

    fn walk_children(
        &mut self,
        dir: &Path,
        source_root: bool,
        kids: Vec<fs::DirEntry>,
        entries: &mut Vec<forge_core::TreeEntry>,
    ) -> Result<()> {
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
            let ft = k.file_type().map_err(|e| io_at(&k.path(), e))?;
            if ft.is_symlink() {
                if !self.options.follow_symlinks {
                    return Err(Error::Invalid(format!(
                        "{SYMLINK_REFUSAL_PREFIX}{}",
                        k.path().display()
                    )));
                }
                entries.push(self.follow_symlink(&k.path(), name)?);
                continue;
            }
            if ft.is_dir() {
                let id = self.walk(&k.path(), false)?;
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
                let id = self.store.put_blob_data(&data)?;
                entries.push(forge_core::TreeEntry {
                    name,
                    kind: forge_types::EntryKind::Blob,
                    id,
                    exec,
                });
            }
        }
        Ok(())
    }

    /// Materialise the CONTENT a symlink points at, under the LINK's name.
    ///
    /// Every refusal below is `Error::Invalid`, never `Error::Io`: a dangling
    /// link, a loop and an escaping target are all caller-supplied input and
    /// must land on the input exit code, not on the internal-failure one.
    fn follow_symlink(&mut self, link: &Path, name: String) -> Result<forge_core::TreeEntry> {
        // `canonicalize` resolves the whole chain, so it is also the loop and
        // dangling-link detector: ELOOP and ENOENT both surface here.
        let target = fs::canonicalize(link).map_err(|e| {
            let reason = match e.kind() {
                std::io::ErrorKind::NotFound => "target does not exist".to_string(),
                _ => e.to_string(),
            };
            Error::Invalid(format!(
                "{SYMLINK_REFUSAL_PREFIX}{}: {reason}",
                link.display()
            ))
        })?;
        // THE CONTAINMENT RULE. Component-wise, so a sibling root whose name
        // merely shares a prefix does not pass. Without this, following a link
        // would copy bytes from outside the import root into the repository.
        if !target.starts_with(&self.root_real) {
            return Err(Error::Invalid(format!(
                "{SYMLINK_REFUSAL_PREFIX}{}: target {} is outside the import root {}",
                link.display(),
                target.display(),
                self.root_real.display()
            )));
        }
        let meta = fs::symlink_metadata(&target).map_err(|e| {
            Error::Invalid(format!("{SYMLINK_REFUSAL_PREFIX}{}: {e}", link.display()))
        })?;
        if meta.is_dir() {
            if let Some(id) = directory_identity(&meta) {
                if self.stack.contains(&id) {
                    return Err(Error::Invalid(format!(
                        "{SYMLINK_REFUSAL_PREFIX}{}: target {} re-enters a directory already being imported (symlink loop)",
                        link.display(),
                        target.display()
                    )));
                }
            }
            let id = self.walk(&target, false)?;
            return Ok(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        }
        if !meta.is_file() {
            return Err(Error::Invalid(format!(
                "{SYMLINK_REFUSAL_PREFIX}{}: target {} is not a regular file or directory",
                link.display(),
                target.display()
            )));
        }
        let (data, exec) = read_import_file(&target)?;
        let id = self.store.put_blob_data(&data)?;
        Ok(forge_core::TreeEntry {
            name,
            kind: forge_types::EntryKind::Blob,
            id,
            exec,
        })
    }
}

/// Open `dir` refusing to traverse a final symlink, and return its identity.
/// `None` on platforms without `O_DIRECTORY|O_NOFOLLOW`, where the caller keeps
/// the pre-existing path-based behaviour.
fn open_directory_nofollow(dir: &Path) -> Result<Option<(u64, u64)>> {
    #[cfg(unix)]
    {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(dir)
            .map_err(|e| match e.raw_os_error() {
                // The name is a symlink now even though the dirent said
                // directory: that is exactly the swap this open exists to catch.
                Some(code) if code == libc::ELOOP || code == libc::ENOTDIR => Error::Invalid(
                    format!("source path changed type during import: {}", dir.display()),
                ),
                _ => io_at(dir, e),
            })?;
        let meta = file.metadata().map_err(|e| io_at(dir, e))?;
        Ok(Some((meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        let meta = fs::symlink_metadata(dir).map_err(|e| io_at(dir, e))?;
        if !meta.is_dir() {
            return Err(Error::Invalid(format!(
                "source path changed type during import: {}",
                dir.display()
            )));
        }
        Ok(None)
    }
}

fn directory_identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// The pathname must still name the directory we enumerated.
fn verify_directory_identity(dir: &Path, pinned: Option<(u64, u64)>) -> Result<()> {
    let Some(pinned) = pinned else {
        return Ok(());
    };
    let meta = fs::symlink_metadata(dir).map_err(|e| io_at(dir, e))?;
    if !meta.is_dir() || directory_identity(&meta) != Some(pinned) {
        return Err(Error::Invalid(format!(
            "source path changed identity during import: {}",
            dir.display()
        )));
    }
    Ok(())
}

/// Turn a first-symlink refusal into a report of every symlink in the tree.
///
/// One symlink per attempt makes preparing a real repository a fix-one-rerun
/// loop. The sweep is lstat-only and runs only on the already-failing path, so
/// a successful import pays nothing for it. It is best effort: if the sweep
/// itself fails, the original single-path refusal stands.
fn symlink_refusal(root: &Path, first: &str) -> String {
    let mut found = Vec::new();
    if sweep_symlinks(root, true, &mut found).is_err() {
        return format!("{first}; pass --follow-symlinks to materialise link targets that stay inside the import root (a VERSION 1 tree cannot represent a symlink; see docs/POSIX.md)");
    }
    found.sort();
    let extra = found.len().saturating_sub(1);
    let named: Vec<String> = found
        .iter()
        .take(SYMLINK_REPORT_LIMIT)
        .map(|p| p.display().to_string())
        .collect();
    let list = if extra == 0 {
        first.to_string()
    } else if found.len() <= SYMLINK_REPORT_LIMIT {
        format!(
            "{SYMLINK_REFUSAL_PREFIX}{} ({extra} more symlink(s) in this tree: {})",
            found[0].display(),
            named[1..].join(", ")
        )
    } else {
        format!(
            "{SYMLINK_REFUSAL_PREFIX}{} ({extra} more symlink(s) in this tree, first {}: {})",
            found[0].display(),
            SYMLINK_REPORT_LIMIT - 1,
            named[1..].join(", ")
        )
    };
    format!("{list}; pass --follow-symlinks to materialise link targets that stay inside the import root (a VERSION 1 tree cannot represent a symlink; see docs/POSIX.md)")
}

fn sweep_symlinks(dir: &Path, source_root: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    if out.len() > SYMLINK_REPORT_LIMIT * 4 {
        return Ok(());
    }
    for kid in import_dir_entries(dir)? {
        let name = kid.file_name();
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        let ft = kid.file_type().map_err(|e| io_at(&kid.path(), e))?;
        if ft.is_symlink() {
            out.push(kid.path());
        } else if ft.is_dir() {
            // Never descends through a link, so the sweep cannot loop.
            sweep_symlinks(&kid.path(), false, out)?;
        }
    }
    Ok(())
}

/// Name the path in every host-filesystem error the import raises.
///
/// `std::io::Error` carries an errno and nothing else, and `From<io::Error>`
/// turns it into a bare `Error::Io`. On a real source tree that produced
/// `io: Permission denied (os error 13)` with no indication of WHICH of tens of
/// thousands of files was unreadable, so the operator had no next step. The
/// variant is deliberately still `Error::Io`: CLI_ABI.md pins the exit code for
/// a host I/O failure and this only adds the missing subject to the sentence.
fn io_at(path: &Path, error: std::io::Error) -> Error {
    Error::Io(format!("{}: {error}", path.display()))
}

fn import_dir_entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut kids: Vec<_> = fs::read_dir(dir)
        .map_err(|e| io_at(dir, e))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| io_at(dir, e))?;
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
    let mut file = options.open(path).map_err(|e| io_at(path, e))?;
    let before = file.metadata().map_err(|e| io_at(path, e))?;
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
    file.read_to_end(&mut data).map_err(|e| io_at(path, e))?;

    // A second read from the same descriptor catches content mutation even on
    // filesystems with coarse timestamp granularity, without allocating a second
    // full-file buffer.
    file.seek(SeekFrom::Start(0)).map_err(|e| io_at(path, e))?;
    let mut offset = 0usize;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| io_at(path, e))?;
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

    let after = file.metadata().map_err(|e| io_at(path, e))?;
    if !import_file_metadata_stable(&before, &after) {
        return Err(Error::Invalid(format!(
            "source file metadata changed during import: {}",
            path.display()
        )));
    }

    // The pathname must still name the same regular file we opened. This closes
    // the common rename/symlink-swap TOCTOU without pretending to provide a host
    // filesystem snapshot primitive.
    let path_after = fs::symlink_metadata(path).map_err(|e| io_at(path, e))?;
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
