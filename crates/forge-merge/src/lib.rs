//! Path-granular 3-way merge. Conflicts are objects, not lost work.

use forge_core::{Conflict, ConflictPath, Tree, TreeEntry};
use forge_store::Store;
use forge_types::{EntryKind, Error, ObjectId, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

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

type EntryIdentity = (ObjectId, u8, bool);

fn entry_identity(entry: &TreeEntry) -> EntryIdentity {
    (entry.id, entry.kind as u8, entry.exec)
}

#[derive(Default)]
struct TreeChanges {
    deleted: BTreeMap<String, TreeEntry>,
    added: BTreeMap<String, TreeEntry>,
}

/// Collect the delete/add frontier between two trees.
///
/// Equal subtree ObjectIds are skipped without reading their contents. Whole
/// subtree additions and deletions stay as one frontier entry, so an exact
/// directory move does not turn into one candidate per descendant. Same-name
/// directory rewrites are descended so moves across existing directories are
/// still visible.
fn tree_changes(store: &Store, base: ObjectId, side: ObjectId) -> Result<TreeChanges> {
    let mut changes = TreeChanges::default();
    let mut stack = vec![(String::new(), base, side)];

    while let Some((prefix, base_id, side_id)) = stack.pop() {
        if base_id == side_id {
            continue;
        }
        let base_tree = store.get_tree(base_id)?;
        let side_tree = store.get_tree(side_id)?;
        let base_map = base_tree.as_map();
        let side_map = side_tree.as_map();
        let mut names = HashSet::new();
        names.extend(base_map.keys().cloned());
        names.extend(side_map.keys().cloned());
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();

        for name in names {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            match (base_map.get(&name), side_map.get(&name)) {
                (Some(before), None) => {
                    changes.deleted.insert(path, (*before).clone());
                }
                (None, Some(after)) => {
                    changes.added.insert(path, (*after).clone());
                }
                (Some(before), Some(after)) if same_entry(before, after) => {}
                (Some(before), Some(after))
                    if before.kind == EntryKind::Tree && after.kind == EntryKind::Tree =>
                {
                    stack.push((path, before.id, after.id));
                }
                (Some(_), Some(_)) => {
                    // A same-path replacement is a modification, not a rename
                    // candidate. The ordinary three-way merge classifies it.
                }
                (None, None) => unreachable!("name came from one of the two trees"),
            }
        }
    }

    Ok(changes)
}

fn count_base_identities(
    store: &Store,
    base: ObjectId,
    wanted: &HashSet<EntryIdentity>,
) -> Result<HashMap<EntryIdentity, usize>> {
    let mut counts = HashMap::new();
    if wanted.is_empty() {
        return Ok(counts);
    }

    let mut stack = vec![base];
    while let Some(tree_id) = stack.pop() {
        let tree = store.get_tree(tree_id)?;
        for entry in &tree.entries {
            let identity = entry_identity(entry);
            if wanted.contains(&identity) {
                let count = counts.entry(identity).or_insert(0usize);
                *count = (*count + 1).min(2);
            }
            if entry.kind == EntryKind::Tree {
                stack.push(entry.id);
            }
        }
    }
    Ok(counts)
}

/// Infer only one-to-one relocations of a unique full entry identity.
///
/// Duplicate source identities and multiple matching destinations are
/// deliberately left ambiguous. A false negative falls back to the normal
/// path merge; a false positive would manufacture a trusted-core conflict.
fn infer_exact_renames(
    changes: &TreeChanges,
    base_counts: &HashMap<EntryIdentity, usize>,
) -> HashMap<String, String> {
    let mut destinations: HashMap<EntryIdentity, Vec<String>> = HashMap::new();
    for (path, entry) in &changes.added {
        destinations
            .entry(entry_identity(entry))
            .or_default()
            .push(path.clone());
    }

    let mut renames = HashMap::new();
    for (source, entry) in &changes.deleted {
        let identity = entry_identity(entry);
        if base_counts.get(&identity) != Some(&1) {
            continue;
        }
        let Some(matches) = destinations.get(&identity) else {
            continue;
        };
        if matches.len() == 1 {
            renames.insert(source.clone(), matches[0].clone());
        }
    }
    renames
}

/// Detect the two otherwise-silent exact-rename shapes without changing
/// FORMAT v1: divergent destinations, and rename versus delete.
///
/// The source is absent from both result trees, so a v1 ConflictPath records
/// a=-, b=-, base=<oid> at that source. The immutable ours and theirs trees
/// retain the destinations; no provenance or heuristic guess is encoded.
fn exact_rename_conflicts(
    store: &Store,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Vec<ConflictPath>> {
    let ours_changes = tree_changes(store, base, ours)?;
    let theirs_changes = tree_changes(store, base, theirs)?;

    let wanted: HashSet<_> = ours_changes
        .deleted
        .values()
        .chain(theirs_changes.deleted.values())
        .map(entry_identity)
        .collect();
    let base_counts = count_base_identities(store, base, &wanted)?;
    let ours_renames = infer_exact_renames(&ours_changes, &base_counts);
    let theirs_renames = infer_exact_renames(&theirs_changes, &base_counts);

    let mut conflicts = Vec::new();
    for (source, base_entry) in &ours_changes.deleted {
        if !theirs_changes.deleted.contains_key(source) {
            continue;
        }
        let ours_destination = ours_renames.get(source);
        let theirs_destination = theirs_renames.get(source);
        if ours_destination.is_none() && theirs_destination.is_none() {
            continue;
        }
        if ours_destination.is_some() && ours_destination == theirs_destination {
            continue;
        }
        conflicts.push(ConflictPath {
            path: source.clone(),
            a: None,
            b: None,
            base: Some(base_entry.id),
        });
    }
    Ok(conflicts)
}

pub fn three_way(
    store: &Store,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<MergeOutcome> {
    let mut paths = match base {
        Some(base) if ours != theirs && ours != base && theirs != base => {
            exact_rename_conflicts(store, base, ours, theirs)?
        }
        _ => Vec::new(),
    };
    let tree = merge_trees(store, "", base, ours, theirs, &mut paths)?;
    paths.sort_by(|left, right| left.path.cmp(&right.path));
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

/// Full tree-entry identity for three-way resolution: two entries are the
/// same entry only if their object id, kind AND executable bit all agree.
fn same_entry(a: &TreeEntry, b: &TreeEntry) -> bool {
    a.id == b.id && a.kind == b.kind && a.exec == b.exec
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
                if g.is_some_and(|g| same_entry(g, x)) {
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
                if g.is_some_and(|g| same_entry(g, x)) {
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
                // Entry identity is (id, kind, exec). Comparing only id+kind
                // made a mode-only change invisible: `ours` always won, the
                // incoming exec bit was dropped with exit 0 and no Conflict,
                // and the merged tree depended on which side was --into.
                // Including exec here fixes both, and keeps the result
                // symmetric: whichever side still matches the base loses.
                if same_entry(x, y) {
                    out.push(x.clone());
                    continue;
                }
                if g.is_some_and(|g| same_entry(g, x)) {
                    out.push(y.clone());
                    continue;
                }
                if g.is_some_and(|g| same_entry(g, y)) {
                    out.push(x.clone());
                    continue;
                }
                if x.kind == EntryKind::Tree && y.kind == EntryKind::Tree {
                    // The base entry is a merge base for this subtree only if it
                    // IS a subtree. When the base held a regular file here and
                    // both sides replaced it with a directory, there is no
                    // common subtree: merge from an empty base. Passing the blob
                    // id down made the recursion read a blob as a tree and fail
                    // the whole merge with Corrupt("not a tree") -- exit code 2,
                    // "corrupt" -- on a healthy repository.
                    let base_subtree = g.filter(|g| g.kind == EntryKind::Tree).map(|g| g.id);
                    let merged =
                        merge_trees(store, &child_path, base_subtree, x.id, y.id, conflicts)?;
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
    fn blob_entry(name: &str, id: ObjectId, exec: bool) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            kind: EntryKind::Blob,
            id,
            exec,
        }
    }

    fn tree(store: &Store, entries: Vec<TreeEntry>) -> ObjectId {
        store.put_tree(&Tree::new(entries).unwrap()).unwrap()
    }

    fn exact_rename_conflict(
        store: &Store,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
    ) -> Conflict {
        match three_way(store, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Conflict(conflict) => conflict,
            MergeOutcome::Tree(_) => panic!("expected an exact-rename conflict"),
        }
    }

    #[test]
    fn divergent_exact_renames_conflict_in_both_directions() {
        let (_d, s) = setup();
        let blob = s.put_blob_data(b"same object").unwrap();
        let base = tree(&s, vec![blob_entry("x", blob, false)]);
        let to_y = tree(&s, vec![blob_entry("y", blob, false)]);
        let to_z = tree(&s, vec![blob_entry("z", blob, false)]);

        for (ours, theirs) in [(to_y, to_z), (to_z, to_y)] {
            let conflict = exact_rename_conflict(&s, base, ours, theirs);
            assert_eq!(conflict.paths.len(), 1);
            assert_eq!(conflict.paths[0].path, "x");
            assert_eq!(conflict.paths[0].a, None);
            assert_eq!(conflict.paths[0].b, None);
            assert_eq!(conflict.paths[0].base, Some(blob));
        }
    }

    #[test]
    fn exact_rename_versus_delete_conflicts_in_both_directions() {
        let (_d, s) = setup();
        let blob = s.put_blob_data(b"same object").unwrap();
        let base = tree(&s, vec![blob_entry("x", blob, false)]);
        let renamed = tree(&s, vec![blob_entry("y", blob, false)]);
        let deleted = tree(&s, vec![]);

        for (ours, theirs) in [(renamed, deleted), (deleted, renamed)] {
            let conflict = exact_rename_conflict(&s, base, ours, theirs);
            assert_eq!(conflict.paths.len(), 1);
            assert_eq!(conflict.paths[0].path, "x");
            assert_eq!(conflict.paths[0].base, Some(blob));
        }
    }

    #[test]
    fn matching_exact_renames_merge_without_conflict() {
        let (_d, s) = setup();
        let blob = s.put_blob_data(b"same object").unwrap();
        let base = tree(&s, vec![blob_entry("x", blob, false)]);
        let renamed = tree(&s, vec![blob_entry("y", blob, false)]);

        match three_way(&s, Some(base), renamed, renamed).unwrap() {
            MergeOutcome::Tree(merged) => assert_eq!(merged, renamed),
            MergeOutcome::Conflict(conflict) => panic!("unexpected {conflict:?}"),
        }
    }

    #[test]
    fn duplicate_identity_is_not_guessed_to_be_a_rename() {
        let (_d, s) = setup();
        let blob = s.put_blob_data(b"duplicate").unwrap();
        let base = tree(
            &s,
            vec![
                blob_entry("keep", blob, false),
                blob_entry("x", blob, false),
            ],
        );
        let ours = tree(
            &s,
            vec![
                blob_entry("keep", blob, false),
                blob_entry("y", blob, false),
            ],
        );
        let theirs = tree(&s, vec![blob_entry("keep", blob, false)]);

        match three_way(&s, Some(base), ours, theirs).unwrap() {
            MergeOutcome::Tree(merged) => {
                let names: Vec<_> = s
                    .get_tree(merged)
                    .unwrap()
                    .entries
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect();
                assert_eq!(names, ["keep", "y"]);
            }
            MergeOutcome::Conflict(conflict) => {
                panic!("duplicate content is ambiguous, not a trusted rename: {conflict:?}")
            }
        }
    }

    #[test]
    fn exact_move_across_existing_directories_is_detected() {
        let (_d, s) = setup();
        let blob = s.put_blob_data(b"same object").unwrap();
        let empty = tree(&s, vec![]);
        let from = tree(&s, vec![blob_entry("x", blob, false)]);
        let ours_to = tree(&s, vec![blob_entry("y", blob, false)]);
        let theirs_to = tree(&s, vec![blob_entry("z", blob, false)]);
        let dir_entry = |name: &str, id| TreeEntry {
            name: name.into(),
            kind: EntryKind::Tree,
            id,
            exec: false,
        };
        let base = tree(&s, vec![dir_entry("from", from), dir_entry("to", empty)]);
        let ours = tree(&s, vec![dir_entry("from", empty), dir_entry("to", ours_to)]);
        let theirs = tree(
            &s,
            vec![dir_entry("from", empty), dir_entry("to", theirs_to)],
        );

        let conflict = exact_rename_conflict(&s, base, ours, theirs);
        assert_eq!(conflict.paths.len(), 1);
        assert_eq!(conflict.paths[0].path, "from/x");
        assert_eq!(conflict.paths[0].base, Some(blob));
    }

    /// I11/I12: a mode-only change on one side is a real change. It must survive
    /// the merge, and the merged tree must not depend on which side is `ours`.
    #[test]
    fn i11_i12_mode_only_change_survives_and_merge_is_direction_symmetric() {
        let (_d, s) = setup();
        let tool = s.put_blob_data(b"#!/bin/sh\n").unwrap();
        let seed = s.put_blob_data(b"seed").unwrap();
        let added = s.put_blob_data(b"added").unwrap();

        // base: tool.sh not executable
        let base = s
            .put_tree(
                &Tree::new(vec![
                    blob_entry("seed.txt", seed, false),
                    blob_entry("tool.sh", tool, false),
                ])
                .unwrap(),
            )
            .unwrap();
        // ours: an unrelated addition, tool.sh untouched
        let ours = s
            .put_tree(
                &Tree::new(vec![
                    blob_entry("a.txt", added, false),
                    blob_entry("seed.txt", seed, false),
                    blob_entry("tool.sh", tool, false),
                ])
                .unwrap(),
            )
            .unwrap();
        // theirs: chmod +x tool.sh and nothing else
        let theirs = s
            .put_tree(
                &Tree::new(vec![
                    blob_entry("seed.txt", seed, false),
                    blob_entry("tool.sh", tool, true),
                ])
                .unwrap(),
            )
            .unwrap();

        let mut conflicts = Vec::new();
        let forward = merge_trees(&s, "", Some(base), ours, theirs, &mut conflicts).unwrap();
        assert!(
            conflicts.is_empty(),
            "a mode change against an unrelated add is not a conflict: {conflicts:?}"
        );

        let merged = s.get_tree(forward).unwrap();
        let entry = merged
            .entries
            .iter()
            .find(|e| e.name == "tool.sh")
            .expect("tool.sh survives the merge");
        assert!(entry.exec, "the incoming exec bit was dropped: {entry:?}");
        assert_eq!(entry.id, tool, "the blob itself must not change");

        // Same two inputs, opposite direction: the merged TREE must be identical.
        // (The merge commit legitimately differs, because its parents differ.)
        let mut mirror_conflicts = Vec::new();
        let mirror = merge_trees(&s, "", Some(base), theirs, ours, &mut mirror_conflicts).unwrap();
        assert!(mirror_conflicts.is_empty(), "{mirror_conflicts:?}");
        assert_eq!(
            forward, mirror,
            "merge result depends on which side is --into; I12 says order comes only from the DAG"
        );
    }
}
