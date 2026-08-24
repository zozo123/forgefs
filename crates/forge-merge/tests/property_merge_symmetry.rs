//! I12 as executable algebra: merge order comes only from the commit parent
//! DAG, never from which side the caller happened to label `ours`.
//!
//! Concretely, `three_way(base, ours, theirs)` and `three_way(base, theirs,
//! ours)` must agree on the RESULTING TREE for a clean merge, and on the set of
//! conflicting paths otherwise. The merge COMMIT legitimately differs -- its
//! parents are ordered -- so this asserts on the tree, which is what actually
//! becomes the integrated content.
//!
//! Deterministic and dependency-free: a SplitMix64 seed drives generation, so
//! a failure names the seed that produced it.

use forge_core::{apply_overlay, Overlay};
use forge_merge::{three_way, MergeOutcome};
use forge_store::Store;
use forge_types::ObjectId;
use tempfile::tempdir;

const CASES: u64 = 250;

/// Blob bodies. Two sides picking the same body produce the same blob id, so
/// convergent identical edits are exercised alongside divergent ones.
const BODIES: [&[u8]; 3] = [b"one", b"two", b"three"];

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

/// State of one path on one side: absent, or a blob carrying an exec bit. The
/// exec bit is part of tree-entry identity, so it has to participate: a
/// mode-only change on one side is precisely the case where an order-sensitive
/// merge silently picks a winner.
fn put(overlay: &mut Overlay, store: &Store, path: &str, rng: &mut Rng) {
    match rng.below(7) {
        0 | 1 => {} // absent on this side
        n => {
            let id = store
                .put_blob_data(BODIES[(n - 2) % BODIES.len()])
                .expect("store accepts a blob");
            let exec = rng.below(2) == 1;
            overlay.insert(path.to_string(), Some((id, exec)));
        }
    }
}

/// One side of the merge. `d` is a regular file on some sides and a directory
/// on others, so file/directory kind conflicts are generated too.
fn side(rng: &mut Rng, store: &Store) -> ObjectId {
    let mut overlay = Overlay::new();
    put(&mut overlay, store, "a", rng);
    put(&mut overlay, store, "b", rng);
    if rng.below(2) == 0 {
        put(&mut overlay, store, "d", rng);
    } else {
        put(&mut overlay, store, "d/x", rng);
        put(&mut overlay, store, "d/y", rng);
    }
    put(&mut overlay, store, "e/f/g", rng);
    put(&mut overlay, store, "e/h", rng);
    apply_overlay(None, &overlay, store).expect("overlay folds onto an empty base")
}

fn conflict_paths(outcome: &MergeOutcome) -> Vec<String> {
    match outcome {
        MergeOutcome::Tree(_) => Vec::new(),
        MergeOutcome::Conflict(c) => {
            let mut v: Vec<String> = c.paths.iter().map(|p| p.path.clone()).collect();
            v.sort();
            v
        }
    }
}

#[test]
fn three_way_merge_tree_is_symmetric_in_ours_and_theirs_i12() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");

    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_3a3a_0000);
        let base = side(&mut rng, &store);
        let ours = side(&mut rng, &store);
        let theirs = side(&mut rng, &store);

        let forward = three_way(&store, Some(base), ours, theirs)
            .unwrap_or_else(|e| panic!("seed {seed}: forward merge failed: {e:?}"));
        let reverse = three_way(&store, Some(base), theirs, ours)
            .unwrap_or_else(|e| panic!("seed {seed}: reverse merge failed: {e:?}"));

        match (&forward, &reverse) {
            (MergeOutcome::Tree(f), MergeOutcome::Tree(r)) => {
                assert_eq!(
                    f, r,
                    "seed {seed}: a clean merge produced a different tree when ours/theirs \
                     were swapped -- integration depends on argument order rather than on \
                     the commit DAG (I12)"
                );
            }
            (MergeOutcome::Conflict(_), MergeOutcome::Conflict(_)) => {
                assert_eq!(
                    conflict_paths(&forward),
                    conflict_paths(&reverse),
                    "seed {seed}: swapping ours/theirs changed which paths conflict (I12)"
                );
            }
            _ => panic!(
                "seed {seed}: swapping ours/theirs turned a clean merge into a conflict, \
                 or a conflict into a clean merge (I12)"
            ),
        }
    }
}

#[test]
fn merging_a_side_against_itself_is_the_identity_i12() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");

    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_1d1d_0000);
        let base = side(&mut rng, &store);
        let ours = side(&mut rng, &store);

        match three_way(&store, Some(base), ours, ours)
            .unwrap_or_else(|e| panic!("seed {seed}: self-merge failed: {e:?}"))
        {
            MergeOutcome::Tree(t) => assert_eq!(
                t, ours,
                "seed {seed}: merging a tree with itself did not return that tree (I12)"
            ),
            MergeOutcome::Conflict(c) => panic!(
                "seed {seed}: merging a tree with itself conflicted on {:?} (I12)",
                c.paths
            ),
        }
    }
}

#[test]
fn merging_an_unchanged_side_takes_the_other_side_i12() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");

    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_2b2b_0000);
        let base = side(&mut rng, &store);
        let theirs = side(&mut rng, &store);

        // ours == base: the only real change is theirs, so a fast-forward is the
        // whole answer, in either argument order.
        let forward = three_way(&store, Some(base), base, theirs)
            .unwrap_or_else(|e| panic!("seed {seed}: merge failed: {e:?}"));
        let reverse = three_way(&store, Some(base), theirs, base)
            .unwrap_or_else(|e| panic!("seed {seed}: merge failed: {e:?}"));

        for (label, outcome) in [("forward", &forward), ("reverse", &reverse)] {
            match outcome {
                MergeOutcome::Tree(t) => assert_eq!(
                    *t, theirs,
                    "seed {seed} ({label}): an unchanged side did not fast-forward (I12)"
                ),
                MergeOutcome::Conflict(c) => panic!(
                    "seed {seed} ({label}): an unchanged side conflicted on {:?} (I12)",
                    c.paths
                ),
            }
        }
    }
}

/// Regression for I12. The base held a regular FILE at `d`; both sides replaced
/// it with a DIRECTORY. There is no common subtree at that path, so the subtree
/// merge has an empty base. Handing the base blob id to the subtree recursion
/// instead made it read a blob as a tree, and the whole merge failed with
/// `Corrupt("not a tree")` -- stable error code "corrupt", CLI exit code 2 --
/// on a repository that was not corrupt at all.
#[test]
fn base_file_replaced_by_a_directory_on_both_sides_merges_from_an_empty_base_i12() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");

    let leaf = |path: &str, body: &[u8]| {
        let id = store.put_blob_data(body).expect("store accepts a blob");
        let mut overlay = Overlay::new();
        overlay.insert(path.to_string(), Some((id, false)));
        apply_overlay(None, &overlay, &store).expect("overlay folds onto an empty base")
    };

    let base = leaf("d", b"d was a regular file");
    let ours = leaf("d/x", b"ours");
    let theirs = leaf("d/y", b"theirs");

    let merged = match three_way(&store, Some(base), ours, theirs)
        .expect("a file replaced by a directory on both sides is a legal merge, not corruption")
    {
        MergeOutcome::Tree(t) => t,
        MergeOutcome::Conflict(c) => {
            panic!(
                "expected a clean directory union, got a conflict on {:?}",
                c.paths
            )
        }
    };

    let top = store.get_tree(merged).expect("merged tree is readable");
    let d = top
        .get("d")
        .expect("merged tree keeps the directory that replaced the file");
    let inner = store.get_tree(d.id).expect("merged subtree is readable");
    let names: Vec<&str> = inner.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["x", "y"],
        "the subtree merge lost a side when the base had no subtree"
    );

    let swapped = match three_way(&store, Some(base), theirs, ours).expect("reverse merge") {
        MergeOutcome::Tree(t) => t,
        MergeOutcome::Conflict(c) => panic!("reverse merge conflicted on {:?}", c.paths),
    };
    assert_eq!(
        merged, swapped,
        "the recovered merge is still order-dependent (I12)"
    );
}
