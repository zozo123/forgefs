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
            return Err(Error::Invalid(
                "commit ancestry exceeds safety limit".into(),
            ));
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
    let common: HashSet<ObjectId> = am
        .keys()
        .filter(|id| bm.contains_key(id))
        .copied()
        .collect();
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
    let a = store.get_tree(ours)?;
    let b = store.get_tree(theirs)?;
    let g = match base {
        Some(id) => store.get_tree(id)?,
        None => Tree::default(),
    };
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
                // A TreeEntry is the merge atom: kind, OID, and executable bit.
                // Treat deletion as clean only when ours is exactly the base entry.
                if g == Some(x) {
                    // deleted on theirs, unchanged ours -> take delete
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
                if g == Some(x) {
                    // deleted on ours, unchanged theirs -> take delete
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
                // Entry identity includes metadata. Comparing only OID/kind silently
                // discarded chmod-only changes and made merge output depend on which
                // side happened to be named `ours`.
                if x == y {
                    out.push(x.clone());
                    continue;
                }
                if g == Some(x) {
                    out.push(y.clone());
                    continue;
                }
                if g == Some(y) {
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

    fn blob(name: &str, id: ObjectId, exec: bool) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            kind: EntryKind::Blob,
            id,
            exec,
        }
    }

    fn put_tree(store: &Store, entries: Vec<TreeEntry>) -> ObjectId {
        store.put_tree(&Tree::new(entries).unwrap()).unwrap()
    }

    fn merged_tree(outcome: MergeOutcome) -> ObjectId {
        match outcome {
            MergeOutcome::Tree(id) => id,
            MergeOutcome::Conflict(conflict) => panic!("unexpected conflict {conflict:?}"),
        }
    }

    #[test]
    fn disjoint_auto_merge() {
        let (_d, s) = setup();
        let ba = s.put_blob_data(b"a").unwrap();
        let bb = s.put_blob_data(b"b").unwrap();
        let bc = s.put_blob_data(b"c").unwrap();
        let base = put_tree(&s, vec![blob("keep.txt", bc, false)]);
        let ours = put_tree(
            &s,
            vec![blob("keep.txt", bc, false), blob("a.txt", ba, false)],
        );
        let theirs = put_tree(
            &s,
            vec![blob("keep.txt", bc, false), blob("b.txt", bb, false)],
        );
        let tree = merged_tree(three_way(&s, Some(base), ours, theirs).unwrap());
        assert_eq!(s.get_tree(tree).unwrap().entries.len(), 3);
    }

    #[test]
    fn same_path_conflict() {
        let (_d, s) = setup();
        let b0 = s.put_blob_data(b"0").unwrap();
        let b1 = s.put_blob_data(b"1").unwrap();
        let b2 = s.put_blob_data(b"2").unwrap();
        let base = put_tree(&s, vec![blob("f.txt", b0, false)]);
        let ours = put_tree(&s, vec![blob("f.txt", b1, false)]);
        let theirs = put_tree(&s, vec![blob("f.txt", b2, false)]);
        match three_way(&s, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Conflict(c) => {
                assert_eq!(c.paths.len(), 1);
                assert_eq!(c.paths[0].path, "f.txt");
            }
            MergeOutcome::Tree(_) => panic!("expected conflict"),
        }
    }

    #[test]
    fn mode_only_change_survives_disjoint_merge_in_both_directions() {
        let (_d, s) = setup();
        let tool = s.put_blob_data(b"#!/bin/sh\n").unwrap();
        let added = s.put_blob_data(b"new\n").unwrap();

        let base = put_tree(&s, vec![blob("tool.sh", tool, false)]);
        let ours = put_tree(
            &s,
            vec![blob("tool.sh", tool, false), blob("a.txt", added, false)],
        );
        let theirs = put_tree(&s, vec![blob("tool.sh", tool, true)]);

        let forward = merged_tree(three_way(&s, Some(base), ours, theirs).unwrap());
        let reverse = merged_tree(three_way(&s, Some(base), theirs, ours).unwrap());

        assert_eq!(
            forward, reverse,
            "I12: clean merge must be direction-independent"
        );
        let merged = s.get_tree(forward).unwrap();
        assert_eq!(merged.entries.len(), 2);
        assert!(merged.get("tool.sh").unwrap().exec);
        assert_eq!(merged.get("a.txt").unwrap().id, added);
    }

    #[test]
    fn conflicting_additions_with_same_bytes_but_different_mode_are_loud() {
        let (_d, s) = setup();
        let tool = s.put_blob_data(b"#!/bin/sh\n").unwrap();
        let base = put_tree(&s, vec![]);
        let ours = put_tree(&s, vec![blob("tool.sh", tool, false)]);
        let theirs = put_tree(&s, vec![blob("tool.sh", tool, true)]);

        match three_way(&s, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Conflict(c) => {
                assert_eq!(c.paths.len(), 1);
                assert_eq!(c.paths[0].path, "tool.sh");
            }
            MergeOutcome::Tree(_) => panic!("mode-divergent additions must conflict"),
        }
    }
}
