use crate::object::MAX_TREE_ENTRIES;
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

    /// Decode-time constructor: reject unsorted or duplicate names (I1).
    pub fn from_canonical(entries: Vec<TreeEntry>) -> Result<Self> {
        for e in &entries {
            validate_name(&e.name)?;
        }
        for w in entries.windows(2) {
            if w[0].name.as_bytes() >= w[1].name.as_bytes() {
                return Err(Error::Corrupt(
                    "tree entries are not strictly sorted unique".into(),
                ));
            }
        }
        Ok(Tree { entries })
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| &self.entries[i])
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
    // Trees are already canonical sorted vectors. Keep that representation and
    // merge only the sparse direct-child edits instead of cloning every entry
    // into a BTreeMap (O(n log n) work for a one-entry edit).
    let base_tree = match base {
        Some(id) => store.get_tree(id)?,
        None => Tree::default(),
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

    // A direct file edit is applied before a nested group in the old algorithm;
    // therefore a same-name nested group sees no tree base and then wins the
    // final entry. Preserve that exact semantic while avoiding the full map.
    let mut edits: BTreeMap<String, Option<TreeEntry>> = BTreeMap::new();
    for (name, op) in &files {
        let edit = op.map(|(id, exec)| TreeEntry {
            name: name.clone(),
            kind: EntryKind::Blob,
            id,
            exec,
        });
        edits.insert(name.clone(), edit);
    }

    for (name, sub) in groups {
        let child_base = if files.contains_key(&name) {
            None
        } else {
            match base_tree.get(&name) {
                Some(e) if e.kind == EntryKind::Tree => Some(e.id),
                _ => None,
            }
        };
        let child_prefix = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let child_id = apply_level(child_base, &child_prefix, &sub, store)?;
        let child_tree = store.get_tree(child_id)?;
        let edit = if child_tree.is_empty() {
            None
        } else {
            Some(TreeEntry {
                name: name.clone(),
                kind: EntryKind::Tree,
                id: child_id,
                exec: false,
            })
        };
        edits.insert(name, edit);
    }

    let entries = merge_sorted_entries(base_tree.entries, edits);
    // Name the directory the caller over-filled (#355). `Tree::encode` refuses
    // this too and is the floor, but it sees only a count; the fold is the last
    // frame that still knows WHICH directory the staged writes landed in, and a
    // refusal naming no path is barely more actionable than exit 2 was.
    if entries.len() as u64 > MAX_TREE_ENTRIES {
        let dir = if prefix.is_empty() { "/" } else { prefix };
        return Err(Error::Invalid(format!(
            "checkin would give {dir} {} entries, more than the {MAX_TREE_ENTRIES} a tree may \
             hold; split it into subdirectories",
            entries.len()
        )));
    }
    let tree = Tree::from_canonical(entries)?;
    store.put_tree(&tree)
}

/// Merge canonical base entries with sorted sparse edits in O(n + m).
fn merge_sorted_entries(
    base: Vec<TreeEntry>,
    edits: BTreeMap<String, Option<TreeEntry>>,
) -> Vec<TreeEntry> {
    let mut out = Vec::with_capacity(base.len().saturating_add(edits.len()));
    let mut base = base.into_iter().peekable();
    let mut edits = edits.into_iter().peekable();

    loop {
        match (base.peek(), edits.peek()) {
            (Some(base_entry), Some((edit_name, _))) => {
                match base_entry.name.as_bytes().cmp(edit_name.as_bytes()) {
                    std::cmp::Ordering::Less => out.push(base.next().unwrap()),
                    std::cmp::Ordering::Equal => {
                        base.next();
                        let (_, edit) = edits.next().unwrap();
                        if let Some(entry) = edit {
                            out.push(entry);
                        }
                    }
                    std::cmp::Ordering::Greater => {
                        let (_, edit) = edits.next().unwrap();
                        if let Some(entry) = edit {
                            out.push(entry);
                        }
                    }
                }
            }
            (Some(_), None) => {
                out.extend(base);
                break;
            }
            (None, Some(_)) => {
                out.extend(edits.filter_map(|(_, edit)| edit));
                break;
            }
            (None, None) => break,
        }
    }

    out
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

    #[test]
    fn direct_file_then_nested_group_preserves_group_wins_semantics() {
        let store = Mem(Mutex::new(HashMap::new()));
        let file = ObjectId([6u8; 32]);
        let nested = ObjectId([7u8; 32]);
        let mut ov = Overlay::new();
        ov.insert("dir".into(), Some((file, false)));
        ov.insert("dir/child.txt".into(), Some((nested, false)));

        let root = apply_overlay(None, &ov, &store).unwrap();
        let tree = store.get_tree(root).unwrap();
        let dir = tree.get("dir").unwrap();
        assert_eq!(dir.kind, EntryKind::Tree);
        let child = store.get_tree(dir.id).unwrap();
        assert_eq!(child.get("child.txt").unwrap().id, nested);
    }

    #[test]
    fn sparse_edit_preserves_canonical_order_and_untouched_entries() {
        let store = Mem(Mutex::new(HashMap::new()));
        let entries: Vec<_> = (0..10_000)
            .map(|i| TreeEntry {
                name: format!("f{i:05}"),
                kind: EntryKind::Blob,
                id: ObjectId([(i % 251) as u8; 32]),
                exec: false,
            })
            .collect();
        let base = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
        let replacement = ObjectId([0xee; 32]);
        let mut ov = Overlay::new();
        ov.insert("f05000".into(), Some((replacement, true)));

        let root = apply_overlay(Some(base), &ov, &store).unwrap();
        let tree = store.get_tree(root).unwrap();
        assert_eq!(tree.entries.len(), 10_000);
        assert_eq!(tree.get("f04999").unwrap().name, "f04999");
        assert_eq!(tree.get("f05000").unwrap().id, replacement);
        assert!(tree.get("f05000").unwrap().exec);
        assert_eq!(tree.get("f05001").unwrap().name, "f05001");
        assert!(tree
            .entries
            .windows(2)
            .all(|w| w[0].name.as_bytes() < w[1].name.as_bytes()));
    }

    #[test]
    fn sorted_tree_get_finds_hits_and_misses() {
        let tree = Tree::new(vec![
            TreeEntry {
                name: "zeta".into(),
                kind: EntryKind::Blob,
                id: ObjectId([1u8; 32]),
                exec: false,
            },
            TreeEntry {
                name: "alpha".into(),
                kind: EntryKind::Blob,
                id: ObjectId([2u8; 32]),
                exec: false,
            },
            TreeEntry {
                name: "middle".into(),
                kind: EntryKind::Blob,
                id: ObjectId([3u8; 32]),
                exec: false,
            },
        ])
        .unwrap();

        assert_eq!(tree.get("alpha").unwrap().id, ObjectId([2u8; 32]));
        assert_eq!(tree.get("middle").unwrap().id, ObjectId([3u8; 32]));
        assert_eq!(tree.get("zeta").unwrap().id, ObjectId([1u8; 32]));
        assert!(tree.get("absent").is_none());
    }
}
