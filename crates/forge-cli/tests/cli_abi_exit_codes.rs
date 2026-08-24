//! CLI_ABI.md is a machine contract: 0 ok, 1 denied/capability/input/not-found,
//! 2 corruption or sealed-state, 3 transient busy, 4 stale/conflict, 5 I/O or
//! internal. Exit 5 means "the machine or the code is broken", so no input a
//! caller controls may produce it -- and clap's own default code 2 must never
//! be mistaken for corruption.

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
