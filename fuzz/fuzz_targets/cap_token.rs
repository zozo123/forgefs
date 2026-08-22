#![no_main]

use forge_cap::{verify, Cap, Op};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(token) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(cap) = Cap::from_token(token) else {
        return;
    };

    // Parsing is never authority. Permission decisions are exercised only for
    // tokens that authenticate under the fixed fuzz key.
    let key = [0x5au8; 32];
    if verify(&key, &cap).is_err() {
        return;
    }

    for op in [
        Op::Read,
        Op::Write,
        Op::Branch,
        Op::Merge,
        Op::Grant,
        Op::Seal,
    ] {
        let _ = cap.allows(op, None, 0);
        let _ = cap.allows(op, Some("main"), 0);
        let _ = cap.allows(op, Some("heads/agents/fuzz/1"), u64::MAX);
    }
});
