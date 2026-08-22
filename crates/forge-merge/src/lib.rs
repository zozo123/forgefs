//! Path-granular 3-way merge. Conflicts are objects, not lost work.

use forge_core::{Conflict, ConflictPath, Tree};
use forge_store::Store;
use forge_types::{EntryKind, ObjectId, Result};
use std::collections::{HashMap, HashSet, VecDeque};

pub fn lca(store: &Store, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    if a == b {
        return Ok(Some(a));
    }
    let mut seen = HashSet::new();
    let mut q = VecDeque::new();
    q.push_back(a);
    while let Some(id) = q.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        if let Ok(c) = store.get_commit(id) {
            for p in c.parents {
                q.push_back(p);
            }
        }
    }
    let mut q = VecDeque::new();
    q.push_back(b);
    let mut seen_b = HashSet::new();
    while let Some(id) = q.pop_front() {
        if !seen_b.insert(id) {
            continue;
        }
        if seen.contains(&id) {
            return Ok(Some(id));
        }
        if let Ok(c) = store.get_commit(id) {
            for p in c.parents {
                q.push_back(p);
            }
        }
    }
    Ok(None)
}

#[derive(Clone, Debug)]
pub enum MergeOutcome {
    Tree(ObjectId),
    Conflict(Conflict),
}

pub fn three_way(
    store: &Store,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<MergeOutcome> {
    let mut paths = Vec::new();
    let tree = merge_trees(store, "", base, ours, theirs, &mut paths)?;
    if paths.is_empty() {
        Ok(MergeOutcome::Tree(tree))
    } else {
        Ok(MergeOutcome::Conflict(Conflict {
            bases: base.into_iter().collect(),
            ours,
            theirs,
            paths,
            causal: vec![],
        }))
    }
}

fn merge_trees(
    store: &Store,
    prefix: &str,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
    conflicts: &mut Vec<ConflictPath>,
) -> Result<ObjectId> {
    if ours == theirs {
        return Ok(ours);
    }
    if Some(ours) == base {
        return Ok(theirs);
    }
    if Some(theirs) == base {
        return Ok(ours);
    }
    let our_tree = store.get_tree(ours).ok();
    let their_tree = store.get_tree(theirs).ok();
    let base_tree = match base {
        Some(id) => store.get_tree(id).ok(),
        None => None,
    };
    if our_tree.is_none() || their_tree.is_none() {
        conflicts.push(ConflictPath {
            path: prefix.to_string(),
            a: Some(ours),
            b: Some(theirs),
            base,
        });
        return Ok(ours);
    }
    let a = our_tree.unwrap();
    let b = their_tree.unwrap();
    let g = base_tree.unwrap_or_else(Tree::default);
    let mut names = HashSet::new();
    for e in a
        .entries
        .iter()
        .chain(b.entries.iter())
        .chain(g.entries.iter())
    {
        names.insert(e.name.clone());
    }
    let am = a.as_map();
    let bm = b.as_map();
    let gm = g.as_map();
    let mut out = Vec::new();
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort();
    for name in names {
        let pa = am.get(&name);
        let pb = bm.get(&name);
        let pg = gm.get(&name);
        let child_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match (pa, pb, pg) {
            (None, None, _) => {}
            (Some(x), None, g) => {
                if g.map(|g| g.id) == Some(x.id) {
                    // deleted on theirs, unchanged ours → take delete
                } else if g.is_none() {
                    out.push(x.clone());
                } else {
                    conflicts.push(ConflictPath {
                        path: child_path,
                        a: Some(x.id),
                        b: None,
                        base: g.map(|g| g.id),
                    });
                }
            }
            (None, Some(x), g) => {
                if g.map(|g| g.id) == Some(x.id) {
                    // deleted on ours
                } else if g.is_none() {
                    out.push(x.clone());
                } else {
                    conflicts.push(ConflictPath {
                        path: child_path,
                        a: None,
                        b: Some(x.id),
                        base: g.map(|g| g.id),
                    });
                }
            }
            (Some(x), Some(y), g) => {
                if x.id == y.id && x.kind == y.kind {
                    out.push(x.clone());
                    continue;
                }
                if g.map(|g| g.id) == Some(x.id) {
                    out.push(y.clone());
                    continue;
                }
                if g.map(|g| g.id) == Some(y.id) {
                    out.push(x.clone());
                    continue;
                }
                if x.kind == EntryKind::Tree && y.kind == EntryKind::Tree {
                    let merged =
                        merge_trees(store, &child_path, g.map(|g| g.id), x.id, y.id, conflicts)?;
                    let mut e = x.clone();
                    e.id = merged;
                    out.push(e);
                } else {
                    conflicts.push(ConflictPath {
                        path: child_path,
                        a: Some(x.id),
                        b: Some(y.id),
                        base: g.map(|g| g.id),
                    });
                    out.push(x.clone());
                }
            }
        }
    }
    store.put_tree(&Tree::new(out)?)
}

pub fn commit_parent_map(
    store: &Store,
    start: ObjectId,
) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let mut m = HashMap::new();
    let mut q = vec![start];
    let mut seen = HashSet::new();
    while let Some(id) = q.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Ok(c) = store.get_commit(id) {
            m.insert(id, c.parents.clone());
            q.extend(c.parents);
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::TreeEntry;
    use forge_types::EntryKind;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Store) {
        let d = tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    #[test]
    fn disjoint_auto_merge() {
        let (_d, s) = setup();
        let ba = s.put_blob_data(b"a").unwrap();
        let bb = s.put_blob_data(b"b").unwrap();
        let bc = s.put_blob_data(b"c").unwrap();
        let base = s
            .put_tree(
                &Tree::new(vec![TreeEntry {
                    name: "keep.txt".into(),
                    kind: EntryKind::Blob,
                    id: bc,
                    exec: false,
                }])
                .unwrap(),
            )
            .unwrap();
        let ours = s
            .put_tree(
                &Tree::new(vec![
                    TreeEntry {
                        name: "keep.txt".into(),
                        kind: EntryKind::Blob,
                        id: bc,
                        exec: false,
                    },
                    TreeEntry {
                        name: "a.txt".into(),
                        kind: EntryKind::Blob,
                        id: ba,
                        exec: false,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let theirs = s
            .put_tree(
                &Tree::new(vec![
                    TreeEntry {
                        name: "keep.txt".into(),
                        kind: EntryKind::Blob,
                        id: bc,
                        exec: false,
                    },
                    TreeEntry {
                        name: "b.txt".into(),
                        kind: EntryKind::Blob,
                        id: bb,
                        exec: false,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        match three_way(&s, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Tree(t) => {
                let tree = s.get_tree(t).unwrap();
                assert_eq!(tree.entries.len(), 3);
            }
            MergeOutcome::Conflict(c) => panic!("unexpected {c:?}"),
        }
    }

    #[test]
    fn same_path_conflict() {
        let (_d, s) = setup();
        let b0 = s.put_blob_data(b"0").unwrap();
        let b1 = s.put_blob_data(b"1").unwrap();
        let b2 = s.put_blob_data(b"2").unwrap();
        let e = |id| TreeEntry {
            name: "f.txt".into(),
            kind: EntryKind::Blob,
            id,
            exec: false,
        };
        let base = s.put_tree(&Tree::new(vec![e(b0)]).unwrap()).unwrap();
        let ours = s.put_tree(&Tree::new(vec![e(b1)]).unwrap()).unwrap();
        let theirs = s.put_tree(&Tree::new(vec![e(b2)]).unwrap()).unwrap();
        match three_way(&s, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Conflict(c) => {
                assert_eq!(c.paths.len(), 1);
                assert_eq!(c.paths[0].path, "f.txt");
            }
            MergeOutcome::Tree(_) => panic!("expected conflict"),
        }
    }
}
