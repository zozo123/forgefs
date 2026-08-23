//! Process-level proof that export observes one immutable snapshot while refs move.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const FILES: usize = 16;
const ROUNDS: usize = 4;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
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

fn init(dir: &Path) -> (PathBuf, PathBuf) {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    (
        dir.join(".forge/keys/root.cap"),
        dir.join(".forge/keys/integrator.cap"),
    )
}

fn rewrite_source(source: &Path, version: usize) {
    fs::create_dir_all(source).unwrap();
    for i in 0..FILES {
        fs::write(
            source.join(format!("file-{i:02}.txt")),
            format!("version-{version}"),
        )
        .unwrap();
    }
}

fn import_source(cell: &Path, root: &Path, source: &Path) {
    let mut cmd = authenticated(cell, root);
    cmd.arg("import")
        .arg(source)
        .arg("--ref")
        .arg("heads/source");
    let result = run(&mut cmd);
    assert!(result.contains("heads/source"), "unexpected import: {result}");
}

fn merge_source(cell: &Path, integrator: &Path) -> Output {
    authenticated(cell, integrator)
        .arg("merge")
        .arg("--into=main")
        .arg("--from=heads/source")
        .output()
        .expect("spawn forge merge")
}

fn extract_tar(tar: &Path, into: &Path) {
    fs::create_dir(into).unwrap();
    let out = Command::new("tar")
        .arg("-xf")
        .arg(tar)
        .arg("-C")
        .arg(into)
        .output()
        .expect("spawn system tar");
    assert!(
        out.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_coherent_export(extracted: &Path, old: usize, new: usize) {
    let first = fs::read_to_string(extracted.join("file-00.txt")).unwrap();
    assert!(
        first == format!("version-{old}") || first == format!("version-{new}"),
        "export contained unexpected version: {first}"
    );
    for i in 1..FILES {
        let value = fs::read_to_string(extracted.join(format!("file-{i:02}.txt"))).unwrap();
        assert_eq!(
            value, first,
            "export mixed ref epochs: file-00={first}, file-{i:02}={value}"
        );
    }
}

#[test]
fn cli_export_racing_main_merge_is_wholly_old_or_wholly_new() {
    let d = tempdir().unwrap();
    let (root, integrator) = init(d.path());
    let source = d.path().join("source");

    rewrite_source(&source, 0);
    import_source(d.path(), &root, &source);
    let initial = merge_source(d.path(), &integrator);
    assert!(
        initial.status.success(),
        "initial merge failed: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    for version in 1..=ROUNDS {
        rewrite_source(&source, version);
        import_source(d.path(), &root, &source);

        let tar = d.path().join(format!("race-{version}.tar"));
        let barrier = Arc::new(Barrier::new(2));

        let export = {
            let barrier = Arc::clone(&barrier);
            let cell = d.path().to_path_buf();
            let cap = root.clone();
            let tar = tar.clone();
            std::thread::spawn(move || {
                barrier.wait();
                authenticated(&cell, &cap)
                    .arg("export")
                    .arg("main")
                    .arg("-o")
                    .arg(&tar)
                    .output()
                    .expect("spawn racing export")
            })
        };
        let integrate = {
            let barrier = Arc::clone(&barrier);
            let cell = d.path().to_path_buf();
            let cap = integrator.clone();
            std::thread::spawn(move || {
                barrier.wait();
                merge_source(&cell, &cap)
            })
        };

        let exported = export.join().expect("join export launcher");
        let merged = integrate.join().expect("join merge launcher");
        assert!(
            exported.status.success(),
            "export failed: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        assert!(
            merged.status.success(),
            "merge failed: {}",
            String::from_utf8_lossy(&merged.stderr)
        );

        let extracted = d.path().join(format!("extracted-{version}"));
        extract_tar(&tar, &extracted);
        assert_coherent_export(&extracted, version - 1, version);
    }

    let mut seal = authenticated(d.path(), &integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("export-race")
        .arg("--attest");
    run(&mut seal);

    let mut verify = authenticated(d.path(), &root);
    verify.arg("verify").arg("export-race");
    run(&mut verify);

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
