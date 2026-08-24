use crate::Forge;
use forge_cap::Cap;
use forge_core::Tree;
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId, Result};
use std::fs::File;
use std::path::Path;
use tar::{Builder, Header};

pub fn export_tar(store: &Store, tree: ObjectId, out: &Path) -> Result<()> {
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
        write_tree(store, &mut b, "", tree)?;
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

fn write_tree(store: &Store, b: &mut Builder<File>, prefix: &str, tree: ObjectId) -> Result<()> {
    let t: Tree = store.get_tree(tree)?;
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
            EntryKind::Tree => write_tree(store, b, &path, e.id)?,
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
        self.check_spec_read(cap, spec)?;
        crate::export::export_tar(&self.store, self.resolve_tree(spec)?, out)
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
