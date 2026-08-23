//! Process-level recovery and corruption tests through the shipped `forge` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &str) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_text(cmd: &mut Command) -> String {
    String::from_utf8(run(cmd)).expect("forge stdout is UTF-8")
}

fn init(dir: &Path) -> PathBuf {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    dir.join(".forge/keys/root.cap")
}

fn open_session(dir: &Path, cap: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    run_text(&mut cmd).trim().to_string()
}

fn read(dir: &Path, cap: &str, ns: &str, path: &str) -> Vec<u8> {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("read").arg("--ns").arg(ns).arg(path);
    run(&mut cmd)
}

fn fsck_full(dir: &Path, cap: &str) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

#[test]
fn cli_session_overlay_survives_process_exit() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let ns = open_session(d.path(), root);

    // Every CLI invocation below is a fresh OS process. The write exits before
    // the read/checkin processes start, so the overlay must live in durable
    // repository metadata rather than process memory.
    let mut write = authenticated(d.path(), root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/resume.txt")
        .arg("--text")
        .arg("survives");
    run(&mut write);

    assert_eq!(read(d.path(), root, &ns, "/resume.txt"), b"survives");

    let mut checkin = authenticated(d.path(), root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("resume after process exit");
    let checked_in = run_text(&mut checkin);
    assert!(checked_in.contains("updated"), "{checked_in}");

    let mut refs = authenticated(d.path(), root);
    refs.arg("refs");
    let refs = run_text(&mut refs);
    assert!(
        refs.contains(&format!("heads/agents/anon/{ns}")),
        "session live ref missing after fresh-process checkin: {refs}"
    );
    fsck_full(d.path(), root);
}

#[test]
fn cli_process_dead_overlay_survives_shared_ref_advance_then_forks() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();

    let mut branch = authenticated(d.path(), root);
    branch.arg("branch").arg("main").arg("heads/hot");
    run(&mut branch);

    let open_shared = |dir: &Path, cap: &str| {
        let mut open = authenticated(dir, cap);
        open.arg("session")
            .arg("open")
            .arg("--from=heads/hot");
        let ns = run_text(&mut open).trim().to_string();
        let mut mount = authenticated(dir, cap);
        mount
            .arg("mount")
            .arg("--ns")
            .arg(&ns)
            .arg("/")
            .arg("heads/hot")
            .arg("--rw");
        run(&mut mount);
        ns
    };

    let survivor = open_shared(d.path(), root);
    let mut write_survivor = authenticated(d.path(), root);
    write_survivor
        .arg("write")
        .arg("--ns")
        .arg(&survivor)
        .arg("/survivor.txt")
        .arg("--text")
        .arg("kept across processes");
    run(&mut write_survivor);

    // Producer process is gone. A fresh process must still see its overlay.
    assert_eq!(
        read(d.path(), root, &survivor, "/survivor.txt"),
        b"kept across processes"
    );

    let winner = open_shared(d.path(), root);
    let mut write_winner = authenticated(d.path(), root);
    write_winner
        .arg("write")
        .arg("--ns")
        .arg(&winner)
        .arg("/winner.txt")
        .arg("--text")
        .arg("moves shared head");
    run(&mut write_winner);
    let mut winner_checkin = authenticated(d.path(), root);
    winner_checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&winner)
        .arg("-m")
        .arg("winner");
    let winner_result = run_text(&mut winner_checkin);
    assert!(
        winner_result.contains("updated heads/hot"),
        "unexpected winner result: {winner_result}"
    );

    // Advancing the shared ref cannot erase the abandoned overlay. Its next
    // checkin is an explicit fork because its pinned base lost the CAS race.
    assert_eq!(
        read(d.path(), root, &survivor, "/survivor.txt"),
        b"kept across processes"
    );
    let mut survivor_checkin = authenticated(d.path(), root);
    survivor_checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&survivor)
        .arg("-m")
        .arg("late survivor");
    let survivor_result = run_text(&mut survivor_checkin);
    assert!(
        survivor_result.contains("forked heads/hot"),
        "delayed shared checkin must fork, got: {survivor_result}"
    );
    fsck_full(d.path(), root);
}

#[test]
fn cli_full_fsck_fails_closed_after_durable_blob_bitrot() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let integrator_path = d.path().join(".forge/keys/integrator.cap");
    let integrator = integrator_path.to_str().unwrap();
    let ns = open_session(d.path(), root);

    let mut write = authenticated(d.path(), root);
    write
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/paper.txt")
        .arg("--text")
        .arg("immutable bytes");
    let blob_oid = run_text(&mut write).trim().to_string();
    assert_eq!(blob_oid.len(), 64, "unexpected ObjectId: {blob_oid}");

    let mut checkin = authenticated(d.path(), root);
    checkin
        .arg("checkin")
        .arg("--ns")
        .arg(&ns)
        .arg("-m")
        .arg("paper");
    assert!(run_text(&mut checkin).contains("updated"));

    let mut merge = authenticated(d.path(), integrator);
    merge
        .arg("merge")
        .arg("--into=main")
        .arg("--from")
        .arg(format!("heads/agents/anon/{ns}"));
    run(&mut merge);

    let mut seal = authenticated(d.path(), integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("v1")
        .arg("--attest");
    run(&mut seal);

    let mut verify = authenticated(d.path(), root);
    verify.arg("verify").arg("v1");
    run(&mut verify);
    fsck_full(d.path(), root);

    let object = d
        .path()
        .join(".forge/objects")
        .join(&blob_oid[0..2])
        .join(&blob_oid[2..4])
        .join(&blob_oid);
    assert!(object.is_file(), "blob object missing at {}", object.display());
    let len = fs::metadata(&object).unwrap().len() as usize;
    fs::write(&object, vec![0u8; len.max(1)]).unwrap();

    let mut broken = authenticated(d.path(), root);
    broken.arg("fsck").arg("--full");
    let broken = output(&mut broken);
    assert_eq!(broken.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&broken.stdout).to_ascii_lowercase();
    let stderr = String::from_utf8_lossy(&broken.stderr).to_ascii_lowercase();
    assert!(
        stdout.contains("failed")
            || stdout.contains("hash")
            || stdout.contains("corrupt")
            || stderr.contains("corrupt")
            || stderr.contains("fsck found"),
        "bitrot was not reported clearly\nstdout={stdout}\nstderr={stderr}"
    );
}
