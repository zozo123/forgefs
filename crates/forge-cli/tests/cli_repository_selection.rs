//! The repository a command acts on is chosen by `--dir`/`FORGE_DIR`, never by
//! a positional path argument. A subcommand field named `dir` silently shares
//! the global `--dir` clap arg id and overwrites it, so `import` used to write
//! to whichever repository sat above the SOURCE path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn forge() -> Command { Command::new(env!("CARGO_BIN_EXE_forge")) }
fn root_cap(dir: &Path) -> String { dir.join(".forge/keys/root.cap").display().to_string() }
fn init(dir: &Path) { let out=forge().arg("init").arg(dir).output().expect("spawn forge"); assert!(out.status.success(), "init failed: {out:?}"); }
fn refs(dir: &Path) -> String { let out=forge().arg("--dir").arg(dir).arg("--cap").arg(root_cap(dir)).arg("refs").output().expect("spawn forge"); assert!(out.status.success(), "refs failed: {out:?}"); String::from_utf8_lossy(&out.stdout).into_owned() }
fn file_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root:&Path,path:&Path,out:&mut Vec<(PathBuf,Vec<u8>)>) { let mut entries=fs::read_dir(path).unwrap().map(|e|e.unwrap()).collect::<Vec<_>>(); entries.sort_by_key(|e|e.file_name()); for entry in entries { let path=entry.path(); if entry.file_type().unwrap().is_dir() { walk(root,&path,out); } else { out.push((path.strip_prefix(root).unwrap().to_path_buf(),fs::read(path).unwrap())); } } }
    let mut out=Vec::new(); walk(root,root,&mut out); out
}

#[test]
fn import_targets_the_dir_repository_not_the_one_above_the_source() {
    let d=tempdir().unwrap(); let target=d.path().join("target-repo"); let elsewhere=d.path().join("elsewhere"); init(&target); init(&elsewhere); let source=elsewhere.join("src"); fs::create_dir_all(&source).unwrap(); fs::write(source.join("f.txt"),b"payload").unwrap();
    let out=forge().arg("--dir").arg(&target).arg("--cap").arg(root_cap(&target)).arg("import").arg(&source).arg("--ref").arg("heads/imported").output().expect("spawn forge");
    assert!(out.status.success(),"import failed: stdout={} stderr={}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr)); assert!(refs(&target).contains("heads/imported")); assert!(!refs(&elsewhere).contains("heads/imported"));
}

#[test]
fn init_positional_path_does_not_shadow_the_global_dir_flag() {
    let d=tempdir().unwrap(); let chosen=d.path().join("chosen"); let out=forge().arg("--dir").arg(d.path().join("ignored")).arg("init").arg(&chosen).output().expect("spawn forge"); assert!(out.status.success(),"init failed: {out:?}"); assert!(chosen.join(".forge/VERSION").is_file()); assert!(!d.path().join("ignored/.forge").exists());
}

#[test]
fn bench_rejects_repository_selectors_before_mutating_any_bytes() {
    let d=tempdir().unwrap(); let repo=d.path().join("victim"); init(&repo); fs::write(repo.join("sentinel"),b"do not delete").unwrap(); let before=file_snapshot(&repo);
    for use_env in [false,true] { let mut cmd=forge(); if use_env { cmd.env("FORGE_DIR",&repo); } else { cmd.arg("--dir").arg(&repo); } let out=cmd.arg("bench").args(["--agents","1","--shared","1","--workers","1"]).output().expect("spawn forge bench"); assert_eq!(out.status.code(),Some(1),"unexpected result: {out:?}"); assert!(String::from_utf8_lossy(&out.stderr).contains("does not accept --dir/FORGE_DIR"),"stderr={}",String::from_utf8_lossy(&out.stderr)); assert_eq!(file_snapshot(&repo),before,"repository bytes changed"); }
}

#[test]
fn bench_uses_only_a_new_explicit_scratch_workspace() {
    let d=tempdir().unwrap(); let existing=d.path().join("existing"); fs::create_dir(&existing).unwrap(); fs::write(existing.join("sentinel"),b"keep").unwrap();
    let rejected=forge().arg("bench").arg("--scratch").arg(&existing).args(["--agents","1","--shared","1","--workers","1"]).output().expect("spawn forge bench"); assert_eq!(rejected.status.code(),Some(1),"result: {rejected:?}"); assert_eq!(fs::read(existing.join("sentinel")).unwrap(),b"keep");
    let scratch=d.path().join("new-benchmark-cell"); let completed=forge().arg("bench").arg("--scratch").arg(&scratch).args(["--agents","1","--shared","1","--workers","1"]).output().expect("spawn forge bench"); assert!(completed.status.success(),"stdout={} stderr={}",String::from_utf8_lossy(&completed.stdout),String::from_utf8_lossy(&completed.stderr)); assert_eq!(fs::read(scratch.join(".forge/VERSION")).unwrap(),b"1\n");
}
