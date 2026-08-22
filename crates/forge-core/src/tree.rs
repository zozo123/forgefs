use forge_types::{EntryKind, Error, ObjectId, Result};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: ObjectId,
    pub exec: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        for e in &entries {
            validate_name(&e.name)?;
        }
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        for w in entries.windows(2) {
            if w[0].name == w[1].name {
                return Err(Error::Invalid(format!("duplicate tree name {}", w[0].name)));
            }
        }
        Ok(Tree { entries })
    }

    pub fn sort(&mut self) {
        self.entries
            .sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn as_map(&self) -> BTreeMap<String, TreeEntry> {
        self.entries
            .iter()
            .cloned()
            .map(|e| (e.name.clone(), e))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(Error::Invalid("tree name length".into()));
    }
    if name == "." || name == ".." {
        return Err(Error::Invalid("tree name . or ..".into()));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(Error::Invalid(format!("illegal tree name {name:?}")));
    }
    if !name.is_ascii() {
        // UTF-8 is allowed; reject only NUL and slash (already checked).
    }
    if std::str::from_utf8(name.as_bytes()).is_err() {
        return Err(Error::Invalid("tree name not utf-8".into()));
    }
    Ok(())
}

/// Overlay: relative path → Some(blob, exec) set, None = tombstone.
pub type Overlay = BTreeMap<String, Option<(ObjectId, bool)>>;

pub trait TreeStore {
    fn get_tree(&self, id: ObjectId) -> Result<Tree>;
    fn put_tree(&self, tree: &Tree) -> Result<ObjectId>;
}

/// Copy-on-write fold of overlay paths onto a base tree.
pub fn apply_overlay(
    base: Option<ObjectId>,
    overlay: &Overlay,
    store: &impl TreeStore,
) -> Result<ObjectId> {
    apply_level(base, "", overlay, store)
}

fn apply_level(
    base: Option<ObjectId>,
    prefix: &str,
    overlay: &Overlay,
    store: &impl TreeStore,
) -> Result<ObjectId> {
    let mut entries: BTreeMap<String, TreeEntry> = match base {
        Some(id) => store.get_tree(id)?.as_map(),
        None => BTreeMap::new(),
    };

    let mut groups: BTreeMap<String, Overlay> = BTreeMap::new();
    let mut files: BTreeMap<String, Option<(ObjectId, bool)>> = BTreeMap::new();

    for (path, op) in overlay {
        let rest = match strip_path_prefix(path, prefix) {
            Some(r) => r,
            None => continue,
        };
        if rest.is_empty() {
            continue;
        }
        if rest.contains('/') {
            let (first, _) = rest.split_once('/').unwrap();
            groups
                .entry(first.to_string())
                .or_default()
                .insert(path.clone(), *op);
        } else {
            files.insert(rest.to_string(), *op);
        }
    }

    for (name, op) in files {
        match op {
            Some((id, exec)) => {
                entries.insert(
                    name.clone(),
                    TreeEntry {
                        name,
                        kind: EntryKind::Blob,
                        id,
                        exec,
                    },
                );
            }
            None => {
                entries.remove(&name);
            }
        }
    }

    for (name, sub) in groups {
        let child_base = match entries.get(&name) {
            Some(e) if e.kind == EntryKind::Tree => Some(e.id),
            _ => None,
        };
        let child_prefix = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let child_id = apply_level(child_base, &child_prefix, &sub, store)?;
        let child_tree = store.get_tree(child_id)?;
        if child_tree.is_empty() {
            entries.remove(&name);
        } else {
            entries.insert(
                name.clone(),
                TreeEntry {
                    name,
                    kind: EntryKind::Tree,
                    id: child_id,
                    exec: false,
                },
            );
        }
    }

    let tree = Tree::new(entries.into_values().collect())?;
    store.put_tree(&tree)
}

/// Strip a directory prefix only on a `/` boundary (`dir` does not match `dir2`).
fn strip_path_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    if path == prefix {
        return Some("");
    }
    if path.len() > prefix.len()
        && path.as_bytes().get(prefix.len()) == Some(&b'/')
        && path.starts_with(prefix)
    {
        return Some(&path[prefix.len() + 1..]);
    }
    None
}

pub fn split_path(path: &str) -> Result<Vec<String>> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Ok(vec![]);
    }
    let mut parts = Vec::new();
    for p in path.split('/') {
        validate_name(p)?;
        parts.push(p.to_string());
    }
    Ok(parts)
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::object::hash_bytes;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Mem(Mutex<HashMap<ObjectId, Tree>>);

    impl TreeStore for Mem {
        fn get_tree(&self, id: ObjectId) -> Result<Tree> {
            self.0
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::NotFound(id.hex()))
        }
        fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
            let bytes = tree.encode()?;
            let id = hash_bytes(&bytes);
            self.0.lock().unwrap().insert(id, tree.clone());
            Ok(id)
        }
    }

    #[test]
    fn overlay_adds_file_on_empty() {
        let store = Mem(Mutex::new(HashMap::new()));
        let blob = ObjectId([1u8; 32]);
        let mut ov = Overlay::new();
        ov.insert("hello.txt".into(), Some((blob, false)));
        let root = apply_overlay(None, &ov, &store).unwrap();
        let t = store.get_tree(root).unwrap();
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.entries[0].name, "hello.txt");
        assert_eq!(t.entries[0].id, blob);
    }

    #[test]
    fn overlay_nested_and_delete() {
        let store = Mem(Mutex::new(HashMap::new()));
        let a = ObjectId([2u8; 32]);
        let b = ObjectId([3u8; 32]);
        let mut ov = Overlay::new();
        ov.insert("dir/a.txt".into(), Some((a, false)));
        ov.insert("dir/b.txt".into(), Some((b, true)));
        let root = apply_overlay(None, &ov, &store).unwrap();
        let t = store.get_tree(root).unwrap();
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.entries[0].kind, EntryKind::Tree);
        let sub = store.get_tree(t.entries[0].id).unwrap();
        assert_eq!(sub.entries.len(), 2);

        let mut ov2 = Overlay::new();
        ov2.insert("dir/a.txt".into(), None);
        let root2 = apply_overlay(Some(root), &ov2, &store).unwrap();
        let t2 = store.get_tree(root2).unwrap();
        let sub2 = store.get_tree(t2.entries[0].id).unwrap();
        assert_eq!(sub2.entries.len(), 1);
        assert_eq!(sub2.entries[0].name, "b.txt");
        assert!(sub2.entries[0].exec);
    }

    #[test]
    fn overlay_prefix_is_path_atomic() {
        let store = Mem(Mutex::new(HashMap::new()));
        let a = ObjectId([4u8; 32]);
        let b = ObjectId([5u8; 32]);
        let mut ov = Overlay::new();
        ov.insert("dir/a.txt".into(), Some((a, false)));
        ov.insert("dir2/b.txt".into(), Some((b, false)));
        let root = apply_overlay(None, &ov, &store).unwrap();
        let t = store.get_tree(root).unwrap();
        let names: Vec<_> = t.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["dir", "dir2"]);
    }
}
