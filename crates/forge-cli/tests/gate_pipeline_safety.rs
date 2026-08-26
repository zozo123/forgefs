//! Gate pipelines run under `set -o pipefail`: a reader that exits before its
//! writer drains can turn a correct gate into SIGPIPE/141.
//!
//! Issue #375 removed the obvious one-line `printf | awk '...; exit'` forms,
//! and #381 caught a multiline `forge refs | awk '...; exit'` that escaped the
//! first review. Keep the rule structural across every script the release gate
//! executes: an awk program may return a status from `END`, after EOF, but it
//! must never execute `exit` while records are still being consumed. `head` is
//! likewise forbidden as a pipeline consumer; read the complete producer and
//! latch the first record instead.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn awk_commands(text: &str) -> Vec<(usize, String)> {
    let mut commands = Vec::new();
    for (offset, _) in text.match_indices("awk ") {
        let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..offset].trim_start().starts_with('#') {
            continue;
        }

        let mut command = String::new();
        let mut in_single_quote = false;
        for ch in text[offset..].chars() {
            if ch == '\'' {
                in_single_quote = !in_single_quote;
            }
            if ch == '\n' && !in_single_quote {
                break;
            }
            command.push(ch);
        }
        commands.push((text[..offset].matches('\n').count() + 1, command));
    }
    commands
}

#[test]
fn gate_pipeline_readers_never_stop_before_eof() {
    let root = repo_root();
    for name in [
        "release-gate.sh",
        "cli-abi-conformance.sh",
        "forge-env-line.sh",
    ] {
        let text = fs::read_to_string(root.join("scripts").join(name)).expect("read gate script");

        for (line_no, command) in awk_commands(&text) {
            if !command.contains("exit") {
                continue;
            }
            assert!(
                command.contains("END"),
                "{name}:{line_no} has an awk reader that may exit before EOF under pipefail: {command}"
            );
        }

        let executable = text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let normalized = executable.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !normalized.contains("| head "),
            "{name} pipes into head under pipefail"
        );
    }
}
