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
    let f = File::create(out)?;
    let mut b = Builder::new(f);
    write_tree(store, &mut b, "", tree)?;
    b.finish().map_err(|e| Error::Io(e.to_string()))?;
    Ok(())
}

fn write_tree(store: &Store, b: &mut Builder<File>, prefix: &str, tree: ObjectId) -> Result<()> {
    let t: Tree = store.get_tree(tree)?;
    if !prefix.is_empty() {
        let mut h = Header::new_ustar();
        h.set_path(format!("{prefix}/"))
            .map_err(|e| Error::Io(e.to_string()))?;
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_username("").ok();
        h.set_groupname("").ok();
        h.set_size(0);
        h.set_cksum();
        b.append(&h, &[] as &[u8])
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
                let mut h = Header::new_ustar();
                if h.set_path(&path).is_err() {
                    h = Header::new_gnu();
                    h.set_path(&path).map_err(|e| Error::Io(e.to_string()))?;
                }
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if e.exec { 0o755 } else { 0o644 });
                h.set_mtime(0);
                h.set_uid(0);
                h.set_gid(0);
                h.set_username("").ok();
                h.set_groupname("").ok();
                h.set_size(data.len() as u64);
                h.set_cksum();
                b.append(&h, data.as_slice())
                    .map_err(|e| Error::Io(e.to_string()))?;
            }
        }
    }
    Ok(())
}
