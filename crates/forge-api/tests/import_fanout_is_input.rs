//! #355 end to end: `forge import` of a directory with more entries than a
//! VERSION 1 tree can hold is a caller input error, exit 1, and the refusal
//! names the offending directory and the limit.
//!
//! It used to exit 2 -- `corrupt: tree fanout exceeds limit` -- for an intact
//! source directory and a repository nothing had yet written to. Exit 2 is
//! reserved for corruption (issue #348); the read-back half of the same limit
//! keeps it, and `forge-store/tests/fanout_input_vs_corruption.rs` holds that
//! half.

use forge_api::Forge;
use forge_core::MAX_TREE_ENTRIES;
use forge_types::Error;
use std::fs::{self, File};
use tempfile::tempdir;

fn fill(dir: &std::path::Path, n: u64) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        File::create(dir.join(format!("f{i:07}"))).unwrap();
    }
}

#[test]
fn import_of_an_over_fanout_directory_is_input_not_corruption() {
    let d = tempdir().unwrap();
    let repo = d.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let f = Forge::init(&repo).unwrap();
    let root = f.root_cap().unwrap();

    let src = d.path().join("src");
    let wide = src.join("wide");
    fill(&wide, MAX_TREE_ENTRIES + 1);

    let Err(err) = f.import_dir(&root, &src, "big") else {
        panic!("a directory too wide for a tree must be refused, not stored")
    };
    assert_eq!(
        err.exit_code(),
        1,
        "a large source directory is the caller's input, not a damaged repository: exit 2 says \
         this repository is corrupt and it is brand new (#348, #355). Got: {err}"
    );
    assert!(
        matches!(err, Error::Invalid(_)),
        "the caller-input side of the limit must be Invalid, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains(&wide.display().to_string()),
        "the refusal must name the offending directory so the caller knows which one to split: \
         {msg}"
    );
    assert!(
        msg.contains(&MAX_TREE_ENTRIES.to_string()),
        "the refusal must name the limit: {msg}"
    );

    // Nothing was published: a refused import is not a half-import.
    assert!(
        f.log(&root, "big", 1).is_err(),
        "a refused import published a ref"
    );

    // Positive control, so the refusal above is a real limit and not a broken
    // import. The exactness of the boundary is proved without a 100_000-file
    // walk by `forge-store/tests/fanout_input_vs_corruption.rs`
    // (`a_tree_at_the_limit_still_encodes`).
    fs::remove_dir_all(&wide).unwrap();
    fill(&src.join("narrow"), 4);
    f.import_dir(&root, &src, "big")
        .expect("an ordinary directory still imports");
}
