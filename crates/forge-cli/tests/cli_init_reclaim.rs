#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> String {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("forge stdout is UTF-8")
}

fn staging_dirs(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".forge.init-"))
        })
        .collect()
}

#[test]
fn retry_after_each_prepublication_crash_reclaims_secret_staging() {
    let phases = [
        "staging-created",
        "directories-created",
        "keys-written",
        "catalog-created",
        "initial-objects-written",
        "main-ref-written",
        "version-written",
        "staging-durable",
    ];

    for phase in phases {
        let d = tempdir().unwrap();
        let crashed = output(
            forge()
                .arg("init")
                .current_dir(d.path())
                .env("FORGEFS_TEST_INIT_CRASH_AFTER", phase),
        );
        assert_eq!(crashed.status.code(), Some(86), "phase={phase}");
        assert!(!d.path().join(".forge").exists(), "phase={phase}");
        assert_eq!(staging_dirs(d.path()).len(), 1, "phase={phase}");

        run(forge().arg("init").current_dir(d.path()));
        assert!(d.path().join(".forge/VERSION").is_file(), "phase={phase}");
        assert!(
            staging_dirs(d.path()).is_empty(),
            "retry leaked staging debris after phase={phase}: {:?}",
            staging_dirs(d.path())
        );
    }
}

#[test]
fn fsck_reports_reserved_init_staging_debris() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let debris = d.path().join(".forge.init-dead-test");
    fs::create_dir_all(debris.join("keys")).unwrap();
    fs::write(debris.join("keys/root.secret"), b"stale-secret").unwrap();

    let root = d.path().join(".forge/keys/root.cap");
    let checked = output(
        forge()
            .arg("--dir")
            .arg(d.path())
            .arg("--cap")
            .arg(&root)
            .arg("fsck")
            .arg("--full"),
    );
    assert!(!checked.status.success());
    let stdout = String::from_utf8_lossy(&checked.stdout);
    assert!(stdout.contains("[INIT_STAGING]"), "{stdout}");
    assert!(stdout.contains(".forge.init-dead-test"), "{stdout}");

    // Re-running init against an already-published cell still performs the
    // reserved-prefix cleanup before reporting "already a forge".
    let retry = output(forge().arg("init").current_dir(d.path()));
    assert!(!retry.status.success());
    assert!(
        !debris.exists(),
        "existing-cell init did not reclaim debris"
    );
}
