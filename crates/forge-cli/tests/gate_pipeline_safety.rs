//! Release-gate pipelines run under `set -o pipefail`: a reader that exits
//! before its writer drains can turn a correct gate into SIGPIPE/141.
//!
//! Issue #375 removed the obvious `printf | awk '...; exit'` forms, but the
//! typed-conflict-ref lookup used a multiline `forge refs | awk '...; exit'`
//! and escaped that review. Keep the rule structural: gate scripts may return
//! a status from `END`, after reading all input, but an awk program must never
//! execute `exit` while records are still being consumed.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

#[test]
fn gate_awk_readers_never_exit_before_eof() {
    let root = repo_root();
    for name in ["release-gate.sh", "cli-abi-conformance.sh"] {
        let text = fs::read_to_string(root.join("scripts").join(name)).expect("read gate script");
        for (line_no, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with('#') || !code.contains("awk ") || !code.contains("exit") {
                continue;
            }
            assert!(
                code.contains("END"),
                "{name}:{} has an awk reader that may exit before EOF under pipefail: {line}",
                line_no + 1
            );
        }
    }
}
