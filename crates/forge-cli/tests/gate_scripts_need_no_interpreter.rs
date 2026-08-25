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

// ---------------------------------------------------------------------------
// Issue #354: the declaration itself, RUN rather than read
// ---------------------------------------------------------------------------
//
// The prose above was true about interpreters and false about `grep`, which is
// its own Debian package and not part of coreutils. One `grep -Eq` sat in
// release-gate.sh while three separate places declared the prerequisites to be
// bash, coreutils, sed and awk. On a machine with exactly that PATH the gate
// printed `grep: command not found`, reported `gate: FAIL gate/conflict-object`
// and wrote `"ok": false` into gate-summary.json: a missing TOOL rendered as a
// failing PRODUCT.
//
// `no_gate_script_invokes_an_interpreter` could never have caught it, because
// it only knows the names it was told to look for -- and nobody thinks to add
// `grep` to a deny-list. The tests below take the opposite shape. They build a
// bin directory holding symlinks to exactly the commands
// `scripts/prereq-lib.sh` declares, nothing else, and run the gates with that
// directory as the entire PATH. Any tool the scripts use and do not declare
// fails them, whatever its name, without anyone having had to anticipate it.

/// Every command `scripts/prereq-lib.sh` declares, read out of the file so the
/// test cannot drift from the list the scripts actually enforce.
fn declared_commands(root: &Path) -> Vec<String> {
    let lib = fs::read_to_string(root.join("scripts/prereq-lib.sh")).expect("read prereq-lib.sh");
    let line = lib
        .lines()
        .find(|line| line.starts_with("GATE_REQUIRED_COMMANDS="))
        .expect("prereq-lib.sh must declare GATE_REQUIRED_COMMANDS");
    let list = line
        .split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(inside, _)| inside)
        .expect("GATE_REQUIRED_COMMANDS must be a double-quoted list");
    let commands: Vec<String> = list.split_whitespace().map(str::to_string).collect();
    assert!(
        commands.iter().any(|c| c == "bash") && commands.iter().any(|c| c == "awk"),
        "the declared list must at least name the shell and awk it is written in: {commands:?}"
    );
    commands
}

/// Resolve one command against the ambient PATH, the way a shell would.
fn resolve_on_path(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

/// A bin directory holding symlinks to exactly `commands`, and nothing else.
/// `skip` is omitted, which is how the "declared but absent" case is built.
fn declared_only_bin(dir: &Path, commands: &[String], skip: Option<&str>) -> PathBuf {
    let bin = dir.join("declared-bin");
    fs::create_dir_all(&bin).expect("declared bin");
    for command in commands {
        if Some(command.as_str()) == skip {
            continue;
        }
        let Some(target) = resolve_on_path(command) else {
            panic!(
                "scripts/prereq-lib.sh declares `{command}`, which is not on this machine's PATH; \
                 the declared list must name commands that exist"
            );
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, bin.join(command)).expect("symlink declared command");
    }
    bin
}

fn run_gate(
    script: &Path,
    forge: &str,
    outdir: &Path,
    bin: &Path,
    tmp: &Path,
) -> std::process::Output {
    Command::new(bin.join("bash"))
        .arg(script)
        .arg(forge)
        .arg(outdir)
        .env_clear()
        .env("PATH", bin)
        .env("HOME", tmp)
        .env("TMPDIR", tmp)
        .output()
        .expect("run gate script")
}

/// The whole release gate, on a PATH that contains the declared prerequisites
/// and nothing else. This is the run that found issue #354, and it fails for
/// any undeclared command, not just the one that was there.
#[test]
fn the_release_gate_runs_on_a_path_holding_only_its_declared_prerequisites() {
    let root = repo_root();
    let dir = tempdir().unwrap();
    let commands = declared_commands(&root);
    let bin = declared_only_bin(dir.path(), &commands, None);
    let outdir = dir.path().join("gate-out");

    let out = run_gate(
        &root.join("scripts/release-gate.sh"),
        env!("CARGO_BIN_EXE_forge"),
        &outdir,
        &bin,
        dir.path(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("command not found"),
        "the gate reached for an undeclared command; declared: {commands:?}\n{stderr}"
    );
    assert!(
        out.status.success(),
        "release-gate.sh failed on its own declared PATH: status {:?}\n{}\n{stderr}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout)
    );

    let summary: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outdir.join("gate-summary.json")).unwrap())
            .expect("gate-summary.json must be JSON");
    assert_eq!(summary["ok"], true, "gate-summary.json: {summary}");
    assert_eq!(
        summary["failures"].as_array().map(Vec::len),
        Some(0),
        "gate-summary.json: {summary}"
    );
}

/// A DECLARED prerequisite that this machine does not have is a harness error
/// with an exit code of its own -- before any assertion runs, so there is
/// nothing for it to be mistaken for.
#[test]
fn a_missing_declared_prerequisite_is_exit_3_and_names_itself() {
    let root = repo_root();
    let dir = tempdir().unwrap();
    let commands = declared_commands(&root);
    // `seq` is used only by the conformance fixtures, so its absence would
    // otherwise surface deep inside a row rather than up front.
    let bin = declared_only_bin(dir.path(), &commands, Some("seq"));
    let outdir = dir.path().join("gate-out");

    let out = run_gate(
        &root.join("scripts/release-gate.sh"),
        env!("CARGO_BIN_EXE_forge"),
        &outdir,
        &bin,
        dir.path(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a missing prerequisite must exit 3, not fail the product: {stderr}"
    );
    assert!(
        stderr.contains("seq"),
        "the message must name what is missing: {stderr}"
    );
    assert!(
        !stderr.contains("gate: FAIL"),
        "no gate assertion may be reported as failed: {stderr}"
    );
    assert!(
        !outdir.join("gate-summary.json").exists(),
        "a gate that could not run must not leave a verdict behind"
    );
}

/// The other direction, and the one that actually happened: a command the
/// scripts use and do NOT declare. A copy of the gate is rewritten to reach for
/// an absent tool exactly where the `grep -Eq` of issue #354 was, and the run
/// must be a harness error rather than a Conflict object that failed to match.
#[test]
fn an_undeclared_command_is_a_harness_error_not_a_gate_failure() {
    let root = repo_root();
    let dir = tempdir().unwrap();
    let commands = declared_commands(&root);
    let bin = declared_only_bin(dir.path(), &commands, None);

    // The scripts resolve their libraries relative to their own directory, so
    // the whole set is copied and only the gate is rewritten.
    let scripts = dir.path().join("scripts");
    fs::create_dir_all(&scripts).expect("scripts dir");
    for entry in fs::read_dir(root.join("scripts")).expect("read scripts") {
        let entry = entry.expect("scripts entry");
        if entry.path().extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let dest = scripts.join(entry.file_name());
        fs::copy(entry.path(), &dest).expect("copy script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).expect("chmod script");
        }
    }
    let gate = scripts.join("release-gate.sh");
    let text = fs::read_to_string(&gate).expect("read gate copy");
    let call = "if ! ere_match \"$want\" \"$CONFLICT_SHOW\"; then";
    assert!(
        text.contains(call),
        "the conflict-object match moved; this test must be re-aimed at it"
    );
    let injected = text.replace(
        call,
        "if ! printf '%s\\n' \"$CONFLICT_SHOW\" | forgefs-undeclared-tool -Eq \"$want\"; then",
    );
    fs::write(&gate, injected).expect("write injected gate");

    let outdir = dir.path().join("gate-out");
    // A summary from an earlier, real run must not survive as the answer.
    fs::create_dir_all(&outdir).expect("outdir");
    fs::write(outdir.join("gate-summary.json"), "{\"ok\": true}").expect("stale summary");

    let out = run_gate(
        &gate,
        env!("CARGO_BIN_EXE_forge"),
        &outdir,
        &bin,
        dir.path(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a missing tool must be a harness error, not exit 1: {stderr}"
    );
    assert!(
        stderr.contains("forgefs-undeclared-tool"),
        "the message must name the command that was not found: {stderr}"
    );
    assert!(
        !stderr.contains("gate: FAIL"),
        "a missing tool must not be reported as a failing gate assertion: {stderr}"
    );
    assert!(
        !outdir.join("gate-summary.json").exists(),
        "a gate that could not run must not leave a verdict, stale or fresh"
    );
}
