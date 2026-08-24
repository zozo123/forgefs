//! Process-level regression tests for stale observations and export/import losslessness.

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

fn grant(dir: &Path, root: &str, agent: &str) -> String {
    let mut cmd = authenticated(dir, root);
    cmd.arg("grant")
        .arg("--ops")
        .arg("read,write,branch")
        .arg("--ref")
        .arg(format!("heads/agents/{agent}/*,main"))
        .arg("--agent")
        .arg(agent);
    run_text(&mut cmd).trim().to_string()
}

fn open_session(dir: &Path, cap: &str, from: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from").arg(from);
    run_text(&mut cmd).trim().to_string()
}

fn write_text(dir: &Path, cap: &str, ns: &str, path: &str, text: &str) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("write")
        .arg("--ns")
        .arg(ns)
        .arg(path)
        .arg("--text")
        .arg(text);
    run(&mut cmd);
}

fn checkin(dir: &Path, cap: &str, ns: &str, message: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("checkin")
        .arg("--ns")
        .arg(ns)
        .arg("-m")
        .arg(message);
    run_text(&mut cmd)
}

fn merge(dir: &Path, integrator: &str, from: &str) {
    let mut cmd = authenticated(dir, integrator);
    cmd.arg("merge").arg("--into=main").arg("--from").arg(from);
    run(&mut cmd);
}

fn fsck_full(dir: &Path, root: &str) {
    let mut cmd = authenticated(dir, root);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

fn read(dir: &Path, cap: &str, ns: &str, path: &str) -> Vec<u8> {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("read").arg("--ns").arg(ns).arg(path);
    run(&mut cmd)
}

#[test]
fn cli_stale_observation_blocks_disjoint_checkin() {
    let d = tempdir().unwrap();
    let root_path = init(d.path());
    let root = root_path.to_str().unwrap();
    let integrator_path = d.path().join(".forge/keys/integrator.cap");
    let integrator = integrator_path.to_str().unwrap();

    let alice = grant(d.path(), root, "alice");
    let bob = grant(d.path(), root, "bob");
    let carol = grant(d.path(), root, "carol");

    let alice_v1 = open_session(d.path(), &alice, "main");
    write_text(d.path(), &alice, &alice_v1, "/doc.txt", "v1");
    assert!(checkin(d.path(), &alice, &alice_v1, "v1").contains("updated"));
    merge(
        d.path(),
        integrator,
        &format!("heads/agents/alice/{alice_v1}"),
    );

    let bob_ns = open_session(d.path(), &bob, "main");
    assert_eq!(read(d.path(), &bob, &bob_ns, "/main/doc.txt"), b"v1");

    let alice_v2 = open_session(d.path(), &alice, "main");
    write_text(d.path(), &alice, &alice_v2, "/doc.txt", "v2");
    assert!(checkin(d.path(), &alice, &alice_v2, "v2").contains("updated"));
    merge(
        d.path(),
        integrator,
        &format!("heads/agents/alice/{alice_v2}"),
    );

    write_text(d.path(), &bob, &bob_ns, "/notes.txt", "notes");
    let mut stale = authenticated(d.path(), &bob);
    stale
        .arg("checkin")
        .arg("--ns")
        .arg(&bob_ns)
        .arg("-m")
        .arg("stale notes");
    let stale = output(&mut stale);
    assert_eq!(stale.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&stale.stderr).to_ascii_lowercase();
    assert!(stderr.contains("stale"), "unexpected stderr: {stderr}");

    let bob_fresh = open_session(d.path(), &bob, "main");
    let mut absent = authenticated(d.path(), &bob);
    absent
        .arg("read")
        .arg("--ns")
        .arg(&bob_fresh)
        .arg("/main/notes.txt");
    let absent = output(&mut absent);
    assert_eq!(
        absent.status.code(),
        Some(1),
        "stale overlay leaked to main"
    );

    let carol_ns = open_session(d.path(), &carol, "main");
    write_text(d.path(), &carol, &carol_ns, "/other.txt", "independent");
    assert!(checkin(d.path(), &carol, &carol_ns, "control").contains("updated"));
    fsck_full(d.path(), root);
}

#[test]
fn cli_export_import_roundtrip_preserves_names_and_bytes() {
    let src = tempdir().unwrap();
    let src_root_path = init(src.path());
    let src_root = src_root_path.to_str().unwrap();
    let integrator_path = src.path().join(".forge/keys/integrator.cap");
    let integrator = integrator_path.to_str().unwrap();
    let ns = open_session(src.path(), src_root, "main");

    write_text(src.path(), src_root, &ns, "/hello.txt", "hello\n");
    write_text(src.path(), src_root, &ns, "/sub/a.txt", "nested");
    write_text(src.path(), src_root, &ns, "/space name.txt", "spaced");

    let binary = [0u8, 1, 2, 0xff, 0, 42];
    let binary_path = src.path().join("payload.bin");
    fs::write(&binary_path, binary).unwrap();
    let mut write_binary = authenticated(src.path(), src_root);
    write_binary
        .arg("write")
        .arg("--ns")
        .arg(&ns)
        .arg("/bin.dat")
        .arg("--file")
        .arg(&binary_path);
    run(&mut write_binary);

    let decomposed = "cafe\u{301}.txt";
    if cfg!(target_os = "linux") {
        write_text(
            src.path(),
            src_root,
            &ns,
            &format!("/{decomposed}"),
            "decomposed",
        );
    }

    assert!(checkin(src.path(), src_root, &ns, "roundtrip").contains("updated"));
    merge(src.path(), integrator, &format!("heads/agents/anon/{ns}"));

    let mut seal = authenticated(src.path(), integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("v1")
        .arg("--attest");
    run(&mut seal);

    let tar_path = src.path().join("v1.tar");
    let mut export = authenticated(src.path(), src_root);
    export.arg("export").arg("tags/v1").arg("-o").arg(&tar_path);
    run(&mut export);
    fsck_full(src.path(), src_root);

    let dst = tempdir().unwrap();
    let dst_root_path = init(dst.path());
    let dst_root = dst_root_path.to_str().unwrap();
    let extracted = dst.path().join("extracted");
    fs::create_dir(&extracted).unwrap();
    let untar = Command::new("tar")
        .arg("-xf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&extracted)
        .output()
        .expect("spawn system tar");
    assert!(
        untar.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&untar.stderr)
    );

    let mut import = authenticated(dst.path(), dst_root);
    import
        .arg("import")
        .arg(&extracted)
        .arg("--ref")
        .arg("heads/import");
    run(&mut import);

    let imported = open_session(dst.path(), dst_root, "heads/import");
    assert_eq!(
        read(dst.path(), dst_root, &imported, "/hello.txt"),
        b"hello\n"
    );
    assert_eq!(
        read(dst.path(), dst_root, &imported, "/sub/a.txt"),
        b"nested"
    );
    assert_eq!(
        read(dst.path(), dst_root, &imported, "/space name.txt"),
        b"spaced"
    );
    assert_eq!(read(dst.path(), dst_root, &imported, "/bin.dat"), binary);
    if cfg!(target_os = "linux") {
        assert_eq!(
            read(dst.path(), dst_root, &imported, &format!("/{decomposed}")),
            b"decomposed"
        );
    }
    fsck_full(dst.path(), dst_root);
}
