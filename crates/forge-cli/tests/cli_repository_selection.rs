//! The repository a command acts on is chosen by `--dir`/`FORGE_DIR`, never by
//! a positional path argument. A subcommand field named `dir` silently shares
//! the global `--dir` clap arg id and overwrites it, so `import` used to write
//! to whichever repository sat above the SOURCE path.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn root_cap(dir: &Path) -> String {
    dir.join(".forge/keys/root.cap").display().to_string()
}

fn init(dir: &Path) {
    let out = forge().arg("init").arg(dir).output().expect("spawn forge");
    assert!(out.status.success(), "init failed: {out:?}");
}

fn refs(dir: &Path) -> String {
    let out = forge()
        .arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(root_cap(dir))
        .arg("refs")
        .output()
        .expect("spawn forge");
    assert!(out.status.success(), "refs failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn import_targets_the_dir_repository_not_the_one_above_the_source() {
    let d = tempdir().unwrap();
    let target = d.path().join("target-repo");
    let elsewhere = d.path().join("elsewhere");

    init(&target);
    // A second, independent repository that happens to contain the source tree.
    // A copy or restored backup of the same forge is the realistic shape here,
    // because identical keys make the misdirection silent rather than noisy.
    init(&elsewhere);
    let source = elsewhere.join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("f.txt"), b"payload").unwrap();

    let out = forge()
        .arg("--dir")
        .arg(&target)
        .arg("--cap")
        .arg(root_cap(&target))
        .arg("import")
        .arg(&source)
        .arg("--ref")
        .arg("heads/imported")
        .output()
        .expect("spawn forge");
    assert!(
        out.status.success(),
        "import failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        refs(&target).contains("heads/imported"),
        "the ref must land in the --dir repository, got:\n{}",
        refs(&target)
    );
    assert!(
        !refs(&elsewhere).contains("heads/imported"),
        "the ref leaked into the repository above the source path:\n{}",
        refs(&elsewhere)
    );
}

#[test]
fn init_positional_path_does_not_shadow_the_global_dir_flag() {
    let d = tempdir().unwrap();
    let chosen = d.path().join("chosen");
    // The positional wins for `init` (that is its documented purpose), but it
    // must not be the same clap arg id as the global flag.
    let out = forge()
        .arg("--dir")
        .arg(d.path().join("ignored"))
        .arg("init")
        .arg(&chosen)
        .output()
        .expect("spawn forge");
    assert!(out.status.success(), "init failed: {out:?}");
    assert!(
        chosen.join(".forge/VERSION").is_file(),
        "init used the wrong path"
    );
    assert!(
        !d.path().join("ignored/.forge").exists(),
        "init created a forge at the --dir path as well"
    );
}
