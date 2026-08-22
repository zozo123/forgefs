//! Path-granular 3-way merge. Conflicts are objects, not lost work.

use forge_core::{Conflict, ConflictPath, Tree};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId, Result};
use std::collections::{HashMap, HashSet};

const MAX_ANCESTRY_COMMITS: usize = 1_000_000;

fn ancestor_map(store: &Store, start: ObjectId) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let mut out = HashMap::new();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if out.contains_key(&id) {
            continue;
        }
        if out.len() >= MAX_ANCESTRY_COMMITS {
            return Err(Error::Invalid("commit ancestry exceeds safety limit".into()));
        }
        let commit = store.get_commit(id)?;
        stack.extend(commit.parents.iter().copied());
        out.insert(id, commit.parents);
    }
    Ok(out)
}

/// Return all best common ancestors, sorted by object id for deterministic output.
/// A best common ancestor is a common ancestor that is not itself an ancestor
/// of another common ancestor.
pub fn merge_bases(store: &Store, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>> {
    if a == b {
        // Validate that the object really is a commit before accepting it as a base.
        store.get_commit(a)?;
        return Ok(vec![a]);
    }

    let am = ancestor_map(store, a)?;
    let bm = ancestor_map(store, b)?;
    let common: HashSet<ObjectId> = am.keys().filter(|id| bm.contains_key(id)).copied().collect();
    if common.is_empty() {
        return Ok(Vec::new());
    }

    // Any common node reachable by following parents from another common node
    // is older than that node and therefore cannot be a best merge base.
    let mut dominated = HashSet::new();
    for &candidate in &common {
        let mut stack = am
            .get(&candidate)
            .cloned()
            .ok_or_else(|| Error::Corrupt("common ancestor missing from ancestry map".into()))?;
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if common.contains(&id) {
                dominated.insert(id);
            }
            if let Some(parents) = am.get(&id) {
                stack.extend(parents.iter().copied());
            }
        }
    }

    let mut bases: Vec<_> = common
        .into_iter()
        .filter(|id| !dominated.contains(id))
        .collect();
    bases.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    Ok(bases)
}

/// Compatibility helper for callers that require one merge base.
/// Multiple best bases are a first-class condition and must not be collapsed
/// by traversal order; callers should use `merge_bases` if they can represent it.
pub fn lca(store: &Store, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    let bases = merge_bases(store, a, b)?;
    match bases.as_slice() {
        [] => Ok(None),
        [base] => Ok(Some(*base)),
        _ => Err(Error::Invalid(format!(
            "multiple best merge bases: {}",
            bases
                .iter()
                .map(ObjectId::hex)
                .collect::<Vec<_>>()
                .join(",")
        ))),
    }
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
    ancestor_map(store, start)
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
