use crate::Forge;
use forge_cap::Cap;
use forge_core::{Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use tar::{Builder, Header};
use unicode_normalization::UnicodeNormalization;

/// Host-adapter policy for names that a target filesystem would merge.
///
/// I16 keeps tree names as exact UTF-8 bytes, so `Foo` and `foo`, and the NFC
/// and NFD spellings of one name, are distinct entries. A tar member name is
/// exact bytes too, so the archive itself is faithful -- the loss happens at
/// extraction, on a case-insensitive or normalizing filesystem (APFS/HFS+ and
/// NTFS by default), where the second member silently overwrites the first.
/// Export is the adapter at that boundary, so export is where the collision is
/// detected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Write the archive even when sibling names collide under case folding or
    /// Unicode canonical equivalence.
    ///
    /// Default is `false`: refusing is the safe direction, because the failure
    /// mode being prevented is silent data loss in the caller's extracted tree,
    /// which no exit code or checksum on the archive would reveal. The opt-out
    /// exists because a tar is not itself case-insensitive: archiving such a
    /// tree for a case-sensitive destination, or for round-tripping back into
    /// ForgeFS, is legitimate. It is a deliberate per-call choice, never a
    /// default, and never inferred from the host running the export -- an
    /// export produced on Linux is usually extracted somewhere else.
    pub allow_name_collisions: bool,
}

pub fn export_tar_with(
    store: &Store,
    tree: ObjectId,
    out: &Path,
    opts: ExportOptions,
) -> Result<()> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Build beside the destination and rename only on success. Writing straight
    // to `out` left a syntactically valid 1024-byte EMPTY tar behind on failure,
    // so a caller that checks for the output file -- a backup or release script,
    // reasonably -- saw a valid-looking archive containing nothing.
    let tmp = tmp_path(out);
    let result = (|| -> Result<()> {
        let f = File::create(&tmp)?;
        let mut b = Builder::new(f);
        write_tree(store, &mut b, "", tree, opts)?;
        b.finish().map_err(|e| Error::Io(e.to_string()))?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, out)?;
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}

fn tmp_path(out: &Path) -> std::path::PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".partial-{}", ulid::Ulid::new()));
    match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => std::path::PathBuf::from(name),
    }
}

/// The key a case-insensitive, normalization-insensitive filesystem would file
/// a name under.
///
/// Lowercasing covers the ASCII folding I16 names explicitly, and std's full
/// Unicode lowercase mapping extends it to the rest of the repertoire for free.
/// NFC afterwards collapses the composed/decomposed spellings; the order is
/// safe because Unicode case mapping preserves canonical equivalence, so
/// canonically equivalent inputs stay canonically equivalent after lowercasing.
///
/// Two distinct names sharing a key really are a collision on such a target;
/// the residual gap is the other direction -- full case folding (Greek final
/// sigma, for instance) is not simple lowercase, so an exotic pair can still
/// slip through. That is a miss, not a false alarm, and it is why this is
/// stated as a detector for the documented folds rather than as a proof of
/// safety on every filesystem.
fn fold_key(name: &str) -> String {
    name.to_lowercase().nfc().collect()
}

/// Names that differ only by an invisible property need their bytes shown, or
/// the error reads as "`café.txt` collides with `café.txt`".
fn describe(name: &str) -> String {
    let hex: Vec<String> = name.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{name:?} [{}]", hex.join(" "))
}

fn collision_kind(a: &str, b: &str) -> &'static str {
    let equivalent = |x: &str, y: &str| x.nfc().collect::<String>() == y.nfc().collect::<String>();
    if equivalent(a, b) {
        "Unicode canonical equivalence (NFC/NFD)"
    } else if a.to_lowercase() == b.to_lowercase() {
        "case folding"
    } else {
        "case folding combined with Unicode canonical equivalence"
    }
}

/// I16: export must detect target-filesystem collisions and fail rather than
/// silently normalize, fold, or overwrite names.
fn check_sibling_names(dir: &str, entries: &[TreeEntry]) -> Result<()> {
    let mut seen: HashMap<String, &str> = HashMap::with_capacity(entries.len());
    for e in entries {
        if let Some(first) = seen.insert(fold_key(&e.name), e.name.as_str()) {
            let dir = if dir.is_empty() { "/" } else { dir };
            return Err(Error::Invalid(format!(
                "export refused: in {dir} the distinct names {} and {} collide under {}. \
                 Extracting this archive on a case-insensitive or normalizing filesystem \
                 (macOS, Windows) would silently overwrite one with the other, and ForgeFS \
                 keeps them distinct (I16). Export with allow_name_collisions to write it \
                 anyway for a case-sensitive destination.",
                describe(first),
                describe(&e.name),
                collision_kind(first, &e.name),
            )));
        }
    }
    Ok(())
}

fn write_tree(
    store: &Store,
    b: &mut Builder<File>,
    prefix: &str,
    tree: ObjectId,
    opts: ExportOptions,
) -> Result<()> {
    let t: Tree = store.get_tree(tree)?;
    if !opts.allow_name_collisions {
        check_sibling_names(prefix, &t.entries)?;
    }
    if !prefix.is_empty() {
        // append_data emits a GNU long-name extension when the path does not fit
        // the 100-byte ustar name field. set_path alone cannot: it fails, and on
        // the directory path it failed with the baffling "paths in archives must
        // have at least one component".
        let mut h = Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_username("").ok();
        h.set_groupname("").ok();
        h.set_size(0);
        b.append_data(&mut h, format!("{prefix}/"), &[] as &[u8])
            .map_err(|e| Error::Io(e.to_string()))?;
    }
    for e in t.entries {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        match e.kind {
            EntryKind::Tree => write_tree(store, b, &path, e.id, opts)?,
            EntryKind::Blob => {
                let data = store.get_blob_data(e.id)?;
                let mut h = Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if e.exec { 0o755 } else { 0o644 });
                h.set_mtime(0);
                h.set_uid(0);
                h.set_gid(0);
                h.set_username("").ok();
                h.set_groupname("").ok();
                h.set_size(data.len() as u64);
                b.append_data(&mut h, &path, data.as_slice())
                    .map_err(|e| Error::Io(e.to_string()))?;
            }
        }
    }
    Ok(())
}

impl Forge {
    pub fn export_tar(&self, cap: &Cap, spec: &str, out: &Path) -> Result<()> {
        self.export_tar_with(cap, spec, out, ExportOptions::default())
    }

    pub fn export_tar_with(
        &self,
        cap: &Cap,
        spec: &str,
        out: &Path,
        opts: ExportOptions,
    ) -> Result<()> {
        self.check_spec_read(cap, spec)?;
        crate::export::export_tar_with(&self.store, self.resolve_tree(spec)?, out, opts)
    }

    pub(crate) fn resolve_tree(&self, spec: &str) -> Result<ObjectId> {
        if let Ok((.., c)) = self.peel_commit(spec) {
            return Ok(c.tree);
        }
        let oid = self.resolve_spec_oid(spec)?;
        match self.store.object_type(oid)? {
            ObjectType::Tree => Ok(oid),
            ObjectType::Snapshot => Ok(self.store.get_snapshot(oid)?.tree),
            _ => Err(Error::Invalid("cannot export".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collision_kind, fold_key};

    #[test]
    fn i16_case_and_normalization_share_a_fold_key() {
        assert_eq!(fold_key("Foo"), fold_key("foo"));
        assert_eq!(fold_key("caf\u{e9}"), fold_key("cafe\u{301}"));
        assert_eq!(fold_key("CAF\u{c9}"), fold_key("cafe\u{301}"));
        assert_ne!(fold_key("cafe"), fold_key("caf\u{e9}"));
        assert_ne!(fold_key("a.txt"), fold_key("b.txt"));
    }

    #[test]
    fn i16_collision_kind_names_the_fold_that_merged_them() {
        assert_eq!(collision_kind("Foo", "foo"), "case folding");
        assert_eq!(
            collision_kind("caf\u{e9}", "cafe\u{301}"),
            "Unicode canonical equivalence (NFC/NFD)"
        );
        assert_eq!(
            collision_kind("CAF\u{c9}", "cafe\u{301}"),
            "case folding combined with Unicode canonical equivalence"
        );
    }
}
