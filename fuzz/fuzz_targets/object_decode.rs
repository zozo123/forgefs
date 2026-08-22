#![no_main]

use forge_core::{parse_file, Blob, Commit, Conflict, Snapshot, Tree};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Every typed decoder must fail closed on arbitrary bytes and must never panic.
    let _ = parse_file(data);
    let _ = Blob::decode(data);
    let _ = Tree::decode(data);
    let _ = Commit::decode(data);
    let _ = Conflict::decode(data);
    let _ = Snapshot::decode(data);
});
