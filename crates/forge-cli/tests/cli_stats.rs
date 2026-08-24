//! CLI_ABI: `forge stats --json` is ONE stable machine-readable document.
//!
//! Shape is the contract; values are not. Every number here is a
//! process-lifetime total whose magnitude depends on the host, so this test
//! asserts the key set, the types, and the declared counter scope, and never
//! an amount. I14: the command still runs behind a loaded capability.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{tempdir, TempDir};

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

const TOP_KEYS: &[&str] = &[
    "api",
    "durability",
    "note",
    "schema_version",
    "scope",
    "sqlite",
    "store",
];
const DURABILITY_KEYS: &[&str] = &["fullfsync", "journal_mode", "read_only", "synchronous"];
const STORE_KEYS: &[&str] = &[
    "barrier_us",
    "dedup_hits",
    "fsync_dir",
    "fsync_dir_us",
    "fsync_file",
    "fsync_file_us",
    "puts",
];
const SQLITE_KEYS: &[&str] = &[
    "accounted_us",
    "busy",
    "cas_denied",
    "cas_forked",
    "cas_noop",
    "cas_updated",
    "lock_acquires",
    "lock_wait_us",
    "txn_count",
    "txn_us",
];
const API_KEYS: &[&str] = &[
    "merge_applied",
    "merge_conflict",
    "sessions_opened",
    "stale_observation",
];

fn repo() -> (TempDir, PathBuf) {
    let d = tempdir().expect("stats tempdir");
    let out = forge()
        .arg("init")
        .arg(d.path())
        .output()
        .expect("spawn forge init");
    assert!(out.status.success(), "init failed: {out:?}");
    let cap = d.path().join(".forge/keys/root.cap");
    (d, cap)
}

fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("section must be a JSON object")
        .keys()
        .cloned()
        .collect()
}

fn assert_u64_section(value: &Value, expected: &[&str]) {
    assert_eq!(keys(value), expected, "section key set is a CLI contract");
    for name in expected {
        assert!(
            value[name].is_u64(),
            "{name} must be a non-negative integer counter, got {}",
            value[name]
        );
    }
}

fn stats_json(dir: &Path, cap: &Path) -> Value {
    let out = forge()
        .args(["--dir", dir.to_str().unwrap()])
        .args(["--cap", cap.to_str().unwrap()])
        .args(["stats", "--json"])
        .output()
        .expect("spawn forge stats");
    assert_eq!(
        out.status.code(),
        Some(0),
        "forge stats --json must succeed\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "forge stats --json must emit one parseable document: {e}\nstdout={}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn stats_json_emits_one_stable_document_for_every_counter() {
    let (d, cap) = repo();
    let doc = stats_json(d.path(), &cap);

    assert_eq!(keys(&doc), TOP_KEYS, "top-level key set is a CLI contract");
    assert_eq!(doc["schema_version"], 1);

    // The scope is inside the document so no consumer has to infer it, and so
    // nothing can quietly reread these totals as per-operation measurements.
    assert_eq!(doc["scope"], "process-lifetime");
    let note = doc["note"].as_str().expect("note must be a string");
    assert!(note.contains("Not per-operation"), "{note}");

    let durability = &doc["durability"];
    assert_eq!(keys(durability), DURABILITY_KEYS);
    assert!(durability["journal_mode"].is_string());
    assert!(durability["synchronous"].is_i64());
    assert!(durability["fullfsync"].is_boolean() || durability["fullfsync"].is_null());
    assert!(durability["read_only"].is_boolean());

    assert_u64_section(&doc["store"], STORE_KEYS);
    assert_u64_section(&doc["sqlite"], SQLITE_KEYS);
    assert_u64_section(&doc["api"], API_KEYS);
}

/// Two reads of an unchanged repository must produce the same key set: the
/// document shape may not depend on which counters happen to be non-zero.
#[test]
fn stats_json_shape_does_not_depend_on_counter_values() {
    let (d, cap) = repo();
    let first = stats_json(d.path(), &cap);
    let second = stats_json(d.path(), &cap);
    assert_eq!(keys(&first), keys(&second));
    assert_eq!(keys(&first["store"]), keys(&second["store"]));
    assert_eq!(keys(&first["sqlite"]), keys(&second["sqlite"]));
    assert_eq!(keys(&first["api"]), keys(&second["api"]));
}

/// The human rendering carries the same scope disclaimer as the JSON.
#[test]
fn stats_text_names_its_counter_scope() {
    let (d, cap) = repo();
    let out = forge()
        .args(["--dir", d.path().to_str().unwrap()])
        .args(["--cap", cap.to_str().unwrap()])
        .arg("stats")
        .output()
        .expect("spawn forge stats");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("scope=process-lifetime"), "{text}");
    assert!(
        text.contains("cas_noop=") || text.contains("noop="),
        "{text}"
    );
}

/// I14: no ambient authority. A metrics read is still an authenticated command.
#[test]
fn stats_requires_a_capability() {
    let (d, _cap) = repo();
    let out = forge()
        .args(["--dir", d.path().to_str().unwrap()])
        .args(["stats", "--json"])
        .env_remove("FORGE_CAP")
        .output()
        .expect("spawn forge stats");
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing capability is the denied/input class\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
