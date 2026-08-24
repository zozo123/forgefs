#![cfg(unix)]
//! I11: overlap is a Conflict object, and CLI_ABI.md fixes that report at exit
//! 4. `scripts/release-gate.sh` phase 4 is the only place that assertion runs
//! against a *shipped* binary: release.yml runs the gate per cross-compiled
//! target, where the cargo suite never executes. An assertion that would pass
//! for a forge whose conflicting merge returns 0 is not a gate, so exercise the
//! gate itself: wrap the real forge in a shim that breaks exactly that one bit
//! of the contract and require the gate to refuse it, by id, in its own
//! machine-readable summary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/forge-cli sits two levels below the repository root")
        .to_path_buf()
}

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn ids(rows: &serde_json::Value, what: &str) -> Vec<String> {
    rows.as_array()
        .unwrap_or_else(|| panic!("gate summary {what} is an array"))
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn release_gate_rejects_same_path_overlap_that_does_not_exit_four() {
    // The gate shells out to python3 for its summary and refuses to start
    // without it; a host that cannot run the gate cannot audit it either.
    if !has("bash") || !has("python3") {
        eprintln!("skipping: release-gate.sh needs bash and python3");
        return;
    }
    let d = tempdir().unwrap();

    // A forge that is correct in every respect except that a conflicting merge
    // reports success. Output is passed through untouched, so the conflict
    // object, the typed conflicts/ ref and the pinned destination ref all still
    // look right -- the exit code is the single broken bit.
    let shim = d.path().join("forge");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             merge=0\n\
             for a in \"$@\"; do [ \"$a\" = merge ] && merge=1; done\n\
             \"{real}\" \"$@\"\n\
             rc=$?\n\
             [ \"$merge\" = 1 ] && [ \"$rc\" -eq 4 ] && exit 0\n\
             exit $rc\n",
            real = env!("CARGO_BIN_EXE_forge"),
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();

    let out_dir = d.path().join("gate-out");
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/release-gate.sh"))
        .arg(&shim)
        .arg(&out_dir)
        .output()
        .expect("run scripts/release-gate.sh");
    let log = format!(
        "gate stdout:\n{}\ngate stderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "release-gate harness error, not a verdict:\n{log}"
    );
    assert!(
        !out.status.success(),
        "release-gate passed a forge whose same-path overlap merge exits 0:\n{log}"
    );

    let summary: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(out_dir.join("gate-summary.json")).expect("gate-summary.json"),
    )
    .expect("gate-summary.json is JSON");
    let failures = ids(&summary["failures"], "failures");
    assert!(
        failures
            .iter()
            .any(|id| id == "gate/overlap-merge-conflict"),
        "phase 4 did not flag the exit-code break; failures={failures:?}\n{log}"
    );
    let passed = ids(&summary["phases_passed"], "phases_passed");
    assert!(
        !passed.iter().any(|id| id == "gate/same-path-overlap"),
        "gate recorded the same-path overlap phase as ok; passed={passed:?}\n{log}"
    );
}
