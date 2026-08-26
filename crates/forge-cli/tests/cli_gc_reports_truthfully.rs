//! Issue #356: `gc` told an operator two things that were not true.
//!
//! 1. Bare `forge gc` answered "collection is not implemented (see
//!    docs/GC.md)". Collection landed in #12's follow-up, `gc --help`
//!    advertises `--collect`, and CLI_ABI.md documents it. Exit 1 was correct
//!    the whole time, which is precisely why the exit-code conformance suite
//!    could not catch it: that suite checks status codes, not sentences.
//! 2. `--collect` counted withheld UNREACHABLE objects as reachable, while
//!    `--dry-run` on the same unchanged repository did not. The collect line
//!    read `16 of 16 objects reachable` beside `withheld: 2 objects`, which is
//!    self-contradictory, and it hid from the operator that the repository
//!    contained unreachable objects at all.
//!
//! Neither is an ABI break -- CLI_ABI.md says consumers must not assert gc
//! amounts -- so nothing here pins a number the contract lets move. What it
//! pins is that the two paths agree, that the arithmetic is internally
//! consistent, and that the diagnostic describes the product as it is.
//!
//! On guarding message text without brittleness: the risk is a test that
//! freezes one approved sentence and then has to be edited every time the
//! wording improves. This one never states the sentence. It first PROVES which
//! modes exist by running them, and only then requires the diagnostic to name
//! the modes it just proved. Rewording is free; letting the message drift away
//! from the product is not.

use serde_json::Value;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn init(dir: &Path) {
    assert!(forge()
        .arg("init")
        .arg(dir)
        .output()
        .unwrap()
        .status
        .success());
}

fn authed(dir: &Path) -> Command {
    let mut c = forge();
    c.arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"));
    c
}

fn stdout_of(cmd: &mut Command, what: &str) -> String {
    let out = cmd.output().expect("spawn forge");
    assert!(
        out.status.success(),
        "{what} failed ({:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

/// A repository holding real garbage: a contended round that forked, with the
/// losing contribution's fork ref abandoned. Its objects are then unreachable
/// and, being seconds old, too young for any legal `--min-age-secs` to collect
/// -- which is exactly the shape that made `--collect` misreport.
fn repo_with_withheld_garbage(dir: &Path) {
    init(dir);
    let run = |args: &[&str]| stdout_of(authed(dir).args(args), &format!("{args:?}"));
    run(&["branch", "main", "shared"]);

    let open = || {
        stdout_of(
            authed(dir).args(["session", "open", "--from=shared"]),
            "session open",
        )
        .trim()
        .to_string()
    };
    let seed = open();
    run(&["mount", "--ns", &seed, "/", "ref:shared", "--rw"]);
    run(&["write", "--ns", &seed, "/seed.txt", "--text", "v0"]);
    run(&["checkin", "--ns", &seed, "-m", "seed"]);

    let winner = open();
    run(&["mount", "--ns", &winner, "/", "ref:shared", "--rw"]);
    let loser = open();
    run(&["mount", "--ns", &loser, "/", "ref:shared", "--rw"]);
    run(&["write", "--ns", &winner, "/w.txt", "--text", "w"]);
    run(&["write", "--ns", &loser, "/l.txt", "--text", "loser-only"]);
    run(&["checkin", "--ns", &winner, "-m", "w"]);

    // The losing checkin forks (I18): "forked <requested> -> <fork> ..."
    let forked = run(&["checkin", "--ns", &loser, "-m", "l"]);
    let fork = forked
        .split_whitespace()
        .nth(3)
        .unwrap_or_else(|| panic!("expected a `forked <ref> -> <fork>` line, got {forked:?}"))
        .to_string();
    assert!(fork.contains("forks/"), "not a fork ref: {fork}");

    run(&["abandon", "session", &loser, "--discard-staged"]);
    run(&["abandon", "fork", &fork]);
}

fn gc_json(dir: &Path, mode: &str) -> Value {
    let text = stdout_of(
        authed(dir).args(["gc", mode, "--min-age-secs", "60", "--json"]),
        mode,
    );
    serde_json::from_str(&text).expect("gc --json must be JSON")
}

fn count(doc: &Value, key: &str) -> u64 {
    doc[key]
        .as_u64()
        .unwrap_or_else(|| panic!("gc report has no numeric {key}: {doc}"))
}

/// The plan and the sweep must describe the same repository the same way.
#[test]
fn a_dry_run_and_a_collect_report_the_same_reachability() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    repo_with_withheld_garbage(&r);

    let plan = gc_json(&r, "--dry-run");

    // The fixture is only meaningful if it really produced unreachable
    // objects. Without this the two paths could agree by both reporting
    // "everything is reachable", which is true of a clean repository and
    // proves nothing at all -- the bug was invisible precisely there.
    let withheld = count(&plan, "withheld_young_objects");
    assert!(
        withheld > 0,
        "fixture produced no withheld garbage, so nothing here is being tested: {plan}"
    );
    assert_eq!(count(&plan, "collectable_objects"), 0, "{plan}");

    let swept = gc_json(&r, "--collect");
    assert_eq!(
        count(&swept, "deleted_objects"),
        0,
        "nothing was old enough to delete, so the repository did not change: {swept}"
    );

    // Same repository, no change between the two runs.
    assert_eq!(
        count(&swept, "scanned_objects"),
        count(&plan, "scanned_objects"),
        "plan {plan}\nsweep {swept}"
    );
    assert_eq!(
        count(&swept, "reachable_objects"),
        count(&plan, "reachable_objects"),
        "--collect and --dry-run disagree about how much of this repository is \
         reachable\nplan {plan}\nsweep {swept}"
    );

    // ... and the sweep's own arithmetic has to hold on its own terms, so a
    // future change that broke BOTH paths identically would still fail here.
    for doc in [&plan, &swept] {
        assert_eq!(
            count(doc, "scanned_objects") - count(doc, "reachable_objects"),
            count(doc, "withheld_young_objects") + count(doc, "collectable_objects"),
            "unreachable objects must be accounted for as withheld or collectable: {doc}"
        );
        assert!(
            count(doc, "reachable_objects") < count(doc, "scanned_objects"),
            "a repository with {withheld} withheld unreachable object(s) cannot be \
             entirely reachable: {doc}"
        );
    }
}

/// The rendered line an operator actually reads must not contradict itself
/// either: `N of N reachable` beside a non-zero withheld count is the sentence
/// that hid the garbage.
#[test]
fn the_collect_summary_line_does_not_contradict_its_own_withheld_count() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    repo_with_withheld_garbage(&r);

    let text = stdout_of(
        authed(&r).args(["gc", "--collect", "--min-age-secs", "60"]),
        "gc --collect",
    );
    let head = text.lines().next().unwrap_or_default().to_string();
    let withheld = text
        .lines()
        .find(|l| l.starts_with("withheld "))
        .unwrap_or_else(|| panic!("no withheld line in:\n{text}"))
        .to_string();
    assert!(
        !withheld.starts_with("withheld (younger than min-age): 0 objects"),
        "fixture produced no withheld garbage:\n{text}"
    );

    // "gc (collect, min-age 60s): R of S objects reachable"
    let numbers: Vec<u64> = head
        .split_whitespace()
        .filter_map(|w| w.parse::<u64>().ok())
        .collect();
    let (reachable, scanned) = (numbers[0], numbers[1]);
    assert!(
        reachable < scanned,
        "`{head}` claims everything is reachable while `{withheld}`:\n{text}"
    );
}

/// The diagnostic for a bare `forge gc` must describe the modes the binary
/// actually has. Which modes those are is established by running them, not by
/// reading the help text or trusting a comment.
#[test]
fn bare_gc_names_the_modes_that_demonstrably_work() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    let mut working = Vec::new();
    for mode in ["--dry-run", "--collect"] {
        let out = authed(&r)
            .args(["gc", mode, "--min-age-secs", "60"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`gc {mode}` is documented and must work: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        working.push(mode);
    }
    assert_eq!(working.len(), 2, "both gc modes must exist");

    let out = authed(&r).args(["gc"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing mode is caller input, which CLI_ABI.md classes as exit 1"
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for mode in &working {
        assert!(
            said.contains(mode),
            "`gc {mode}` works, so the diagnostic for a bare `gc` must offer it: {said}"
        );
    }
    for lie in [
        "not implemented",
        "unimplemented",
        "supports --dry-run only",
    ] {
        assert!(
            !said.contains(lie),
            "the diagnostic claims `{lie}` about a command whose modes this test just \
             ran successfully: {said}"
        );
    }
}
