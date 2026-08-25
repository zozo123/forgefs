//! Issue #346: `scripts/release-gate.sh` and `scripts/cli-abi-conformance.sh`
//! exited 2 -- the code CLI_ABI.md reserves for corruption -- with
//! `harness error: python3 is required` on any machine without an interpreter,
//! a base Debian image included. Those two scripts, and
//! `scripts/forge-env-line.sh` which the gate calls, ARE the project's
//! self-verification story: they are what a cautious user runs before trusting
//! a release, so they must need less than the product they verify, not more.
//!
//! Everything the interpreter did -- JSON shaping, one byte-fill, one SQLite
//! header read -- is done in shell and awk now (`scripts/json-lib.sh`). These
//! tests hold that line: they run the scripts with a `python3` on PATH that
//! refuses to do anything except record that it was called, and then check
//! both that nothing called it and that the JSON artifacts are still JSON.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// A PATH whose first entry answers `python3`, `python` and `sqlite3` by
/// recording the call and failing. Any script that still reaches for an
/// interpreter is caught red-handed instead of silently succeeding on a
/// developer machine that happens to have one.
fn poisoned_path(dir: &Path) -> (String, PathBuf) {
    let bin = dir.join("poison-bin");
    fs::create_dir_all(&bin).expect("poison bin");
    let marker = dir.join("interpreter-was-called.log");
    for name in ["python3", "python", "sqlite3", "perl"] {
        let shim = bin.join(name);
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> '{}'\nexit 127\n",
                marker.display()
            ),
        )
        .expect("write shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        }
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (path, marker)
}

fn assert_no_interpreter(marker: &Path) {
    if let Ok(calls) = fs::read_to_string(marker) {
        panic!("a gate script still shells out to an interpreter:\n{calls}");
    }
}

#[test]
fn the_cli_abi_gate_runs_and_reports_json_without_an_interpreter() {
    let dir = tempdir().unwrap();
    let (path, marker) = poisoned_path(dir.path());
    let outdir = dir.path().join("abi-out");

    let out = Command::new("bash")
        .arg(repo_root().join("scripts/cli-abi-conformance.sh"))
        .arg(env!("CARGO_BIN_EXE_forge"))
        .arg(&outdir)
        .env("PATH", &path)
        .env("TMPDIR", dir.path())
        .output()
        .expect("run cli-abi-conformance.sh");
    assert_no_interpreter(&marker);
    assert!(
        out.status.success(),
        "conformance script failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outdir.join("abi-conformance.json")).unwrap())
            .expect("abi-conformance.json must be JSON");
    assert_eq!(report["contract"], "CLI_ABI.md");
    assert_eq!(report["ok"], true);
    assert_eq!(report["blocking_failures"], 0);
    let rows = report["rows"].as_array().expect("rows array");
    assert!(
        rows.len() > 20,
        "expected the full row set, got {}",
        rows.len()
    );
    assert_eq!(
        report["rows_total"].as_u64().unwrap() as usize,
        rows.len(),
        "rows_total must count the rows actually written"
    );

    // The bitrot fixture is the one row that used to need the interpreter to
    // build it, so it is the one that proves the replacement really damaged
    // the object rather than quietly doing nothing.
    let bitrot = rows
        .iter()
        .find(|row| row["id"] == "abi/2-bitrot-fails-closed")
        .expect("the bitrot row must be present");
    assert_eq!(
        bitrot["observed_exit"], 2,
        "a zero-filled object must still be corruption: {bitrot}"
    );

    // Every row's `output` survived JSON escaping as a string, including the
    // ones carrying quotes, braces and embedded newlines from forge's own
    // JSON output.
    for row in rows {
        assert!(
            row["output"].is_string(),
            "row output must be a JSON string: {row}"
        );
    }
}

#[test]
fn the_environment_line_is_valid_json_without_an_interpreter() {
    let dir = tempdir().unwrap();
    let (path, marker) = poisoned_path(dir.path());
    let repo = dir.path().join("repo");

    assert!(Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());

    let out = Command::new("bash")
        .arg(repo_root().join("scripts/forge-env-line.sh"))
        .arg("--json")
        .arg("--forge")
        .arg(env!("CARGO_BIN_EXE_forge"))
        .arg(&repo)
        .env("PATH", &path)
        .output()
        .expect("run forge-env-line.sh --json");
    assert_no_interpreter(&marker);
    assert!(
        out.status.success(),
        "forge-env-line.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json must emit one JSON object");
    for key in [
        "arch",
        "commit",
        "cpu_model",
        "forge_version",
        "os_name",
        "ram_bytes",
        "sqlite_journal_mode",
    ] {
        assert!(doc.get(key).is_some(), "missing {key} in {doc}");
    }
    // Read straight out of the SQLite header this repository just wrote, with
    // no library to ask. `Meta::open` establishes WAL and fails closed
    // without it, so anything else here means the header read is wrong.
    assert_eq!(
        doc["sqlite_journal_mode"], "WAL",
        "the header read must observe the WAL this repository is in: {doc}"
    );
}

/// The declaration itself, enforced. A prose promise about prerequisites is
/// worth what it costs to break, so this reads the scripts and fails if any
/// executable line reaches for an interpreter or a database client again.
#[test]
fn no_gate_script_invokes_an_interpreter() {
    let root = repo_root();
    for name in [
        "release-gate.sh",
        "cli-abi-conformance.sh",
        "forge-env-line.sh",
        "json-lib.sh",
    ] {
        let path = root.join("scripts").join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        for (number, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with('#') || code.is_empty() {
                continue;
            }
            for forbidden in ["python", "sqlite3", "perl ", "ruby ", "node "] {
                assert!(
                    !code.contains(forbidden),
                    "{name}:{} reaches for `{forbidden}`; the gates must need only \
                     bash, coreutils, sed and awk (issue #346): {line}",
                    number + 1
                );
            }
        }
    }
}
