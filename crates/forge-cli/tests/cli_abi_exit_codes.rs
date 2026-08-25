//! CLI_ABI.md is a machine contract: 0 ok, 1 denied/capability/input/not-found,
//! 2 corruption or sealed-state, 3 transient busy, 4 stale/conflict, 5 I/O or
//! internal. Exit 5 means "the machine or the code is broken", so no input a
//! caller controls may produce it -- and clap's own default code 2 must never
//! be mistaken for corruption.

use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn code(cmd: &mut Command) -> i32 {
    let out = cmd.output().expect("spawn forge");
    out.status.code().unwrap_or_else(|| {
        panic!(
            "forge died by signal: {out:?}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
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

fn cap(dir: &Path) -> String {
    dir.join(".forge/keys/root.cap").display().to_string()
}

fn authed(dir: &Path) -> Command {
    let mut c = forge();
    c.arg("--dir").arg(dir).arg("--cap").arg(cap(dir));
    c
}

#[test]
fn clap_usage_errors_are_input_errors_not_corruption() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    for argv in [
        vec!["no-such-subcommand"],
        vec!["refs", "--bogus-flag"],
        vec!["read", "--ns"],
    ] {
        let got = code(authed(&r).args(&argv));
        assert_eq!(got, 1, "argv {argv:?} must be an input error, got {got}");
    }
    // No arguments at all is still a usage error, not corruption.
    assert_eq!(code(&mut forge()), 1);
    // An explicit request for help or version succeeds.
    assert_eq!(code(forge().arg("--help")), 0);
    assert_eq!(code(forge().arg("--version")), 0);
}

#[test]
fn caller_controlled_input_never_produces_exit_five() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    // A cap file that is not valid UTF-8 is bad input, not an I/O fault.
    let bad_cap = d.path().join("bad.cap");
    fs::write(&bad_cap, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(
        code(
            forge()
                .arg("--dir")
                .arg(&r)
                .arg("--cap")
                .arg(&bad_cap)
                .arg("refs")
        ),
        1
    );

    // A --file path the caller got wrong.
    let ns = String::from_utf8(
        authed(&r)
            .args(["session", "open", "--from=main"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(
        code(authed(&r).args([
            "write",
            "--ns",
            &ns,
            "/p.txt",
            "--file",
            d.path().join("absent.bin").to_str().unwrap(),
        ])),
        1
    );

    // Importing a plain file where a directory is required.
    let plain = d.path().join("plain.txt");
    fs::write(&plain, b"x").unwrap();
    assert_eq!(
        code(authed(&r).args(["import", plain.to_str().unwrap(), "--ref", "heads/i"])),
        1
    );
}

#[test]
fn absent_things_are_not_found_not_silent_success() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    // `log` used to exit 0 with no output, so "no history" and "no such ref"
    // were indistinguishable.
    assert_eq!(code(authed(&r).args(["log", "no/such/ref"])), 1);

    // A well-formed but absent ObjectId is not a landmark.
    assert_eq!(
        code(authed(&r).args([
            "landmark",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])),
        1
    );
}

#[test]
fn mount_refuses_an_unresolvable_spec_and_leaves_fsck_clean() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);
    let ns = String::from_utf8(
        authed(&r)
            .args(["session", "open", "--from=main"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    // Persisting this mount made `fsck --full` report MOUNT_REF corruption on a
    // repository whose bytes were intact.
    assert_eq!(
        code(authed(&r).args(["mount", "--ns", &ns, "/m", "no-such-ref-at-all"])),
        1
    );
    assert_eq!(code(authed(&r).args(["fsck", "--full"])), 0);
}

#[test]
fn duplicate_names_are_input_errors_and_a_frozen_tag_is_sealed_state() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    assert_eq!(code(authed(&r).args(["branch", "main", "heads/dup"])), 0);
    // Was exit 5 with a leaked "UNIQUE constraint failed: refs.name".
    assert_eq!(code(authed(&r).args(["branch", "main", "heads/dup"])), 1);

    let integrator = r.join(".forge/keys/integrator.cap").display().to_string();
    let seal = || {
        code(
            forge()
                .arg("--dir")
                .arg(&r)
                .arg("--cap")
                .arg(&integrator)
                .args(["seal", "main", "--tag", "dup-tag"]),
        )
    };
    assert_eq!(seal(), 0);
    // A frozen tag is sealed state, which CLI_ABI.md classes as 2.
    assert_eq!(seal(), 2);
}

/// Issue #348: exit 2 means corruption, so a healthy repository from an older
/// release must never receive it. `fsck --full` is the command a careful
/// operator runs *before* deciding to upgrade, and it is the only read-only
/// command that opens through the ledger-deferred path, so it was the only one
/// that got this wrong: `verify` and reachable `fsck` already exit 1 here.
#[test]
fn an_unmigrated_catalog_is_an_input_error_not_corruption() {
    let d = tempdir().unwrap();
    let r = d.path().join("r");
    init(&r);

    // Give the repository real content, then put its catalog back into the
    // schema-2 shape v0.2.1 wrote: the ledger stops at 2 and `mounts` has no
    // `base_oid` column. Not one object file changes.
    let ns = String::from_utf8(
        authed(&r)
            .args(["session", "open", "--from=main"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(authed(&r)
        .args(["write", "--ns", &ns, "/kept.txt", "--text", "kept"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(authed(&r)
        .args(["checkin", "--ns", &ns, "-m", "fixture"])
        .output()
        .unwrap()
        .status
        .success());

    let conn = Connection::open(r.join(".forge/meta.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE mounts_v2 (
           ns_id TEXT NOT NULL,
           path  TEXT NOT NULL,
           spec  TEXT NOT NULL,
           mode  TEXT NOT NULL CHECK(mode IN ('ro','rw')),
           PRIMARY KEY (ns_id, path)
         );
         INSERT INTO mounts_v2 (ns_id, path, spec, mode)
           SELECT ns_id, path, spec, mode FROM mounts;
         DROP TABLE mounts;
         ALTER TABLE mounts_v2 RENAME TO mounts;
         DELETE FROM schema_migrations WHERE version > 2;",
    )
    .unwrap();
    drop(conn);

    for argv in [
        vec!["fsck", "--full"],
        vec!["fsck", "--full", "--json"],
        vec!["fsck"],
        vec!["verify", "whatever"],
    ] {
        let got = code(authed(&r).args(&argv));
        assert_eq!(
            got, 1,
            "an intact un-migrated repository is not corrupt: {argv:?} exited {got}"
        );
    }

    // One read-write open migrates it, and only then is the audit meaningful.
    assert_eq!(code(authed(&r).arg("refs")), 0);
    assert_eq!(code(authed(&r).args(["fsck", "--full"])), 0);
}
