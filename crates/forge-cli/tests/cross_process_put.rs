//! Process-level proof that write-once object publication converges across processes.

use forge_core::{hash_bytes, Blob};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const PAYLOAD_BYTES: usize = 256 * 1024;
const SAME_WRITERS: usize = 16;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &str, cap: &str) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().expect("spawn forge");
    assert_success(&out);
    out.stdout
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn open_session(dir: &str, cap: &str) -> String {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("session").arg("open").arg("--from=main");
    String::from_utf8(run(&mut cmd)).unwrap().trim().to_string()
}

fn write_command(dir: &str, cap: &str, ns: &str, path: &str, file: &Path) -> Command {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("write")
        .arg("--ns")
        .arg(ns)
        .arg(path)
        .arg("--file")
        .arg(file);
    cmd
}

fn blob_id(data: &[u8]) -> forge_types::ObjectId {
    hash_bytes(
        &Blob {
            data: data.to_vec(),
        }
        .encode(),
    )
}

fn count_named_files(root: &Path, name: &str) -> usize {
    let mut count = 0;
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            count += count_named_files(&entry.path(), name);
        } else if ty.is_file() && entry.file_name() == name {
            count += 1;
        }
    }
    count
}

fn race_writes(
    dir: &str,
    cap: &str,
    sessions: &[String],
    file: &Path,
    path_prefix: &str,
) -> Vec<Output> {
    let barrier = Arc::new(Barrier::new(sessions.len()));
    let mut launchers = Vec::with_capacity(sessions.len());
    for (i, ns) in sessions.iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let dir = dir.to_string();
        let cap = cap.to_string();
        let ns = ns.clone();
        let file = file.to_path_buf();
        let path = format!("/{path_prefix}-{i}.bin");
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            write_command(&dir, &cap, &ns, &path, &file)
                .output()
                .expect("spawn concurrent forge write")
        }));
    }
    launchers
        .into_iter()
        .map(|launcher| launcher.join().unwrap())
        .collect()
}

fn assert_write_oid(outputs: &[Output], expected: &str) {
    for out in outputs {
        assert_success(out);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    }
}

fn read_bytes(dir: &str, cap: &str, ns: &str, path: &str) -> Vec<u8> {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("read").arg("--ns").arg(ns).arg(path);
    run(&mut cmd)
}

fn fsck_full(dir: &str, cap: &str) {
    let mut cmd = authenticated(dir, cap);
    cmd.arg("fsck").arg("--full");
    run(&mut cmd);
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, data).unwrap();
    path
}

#[test]
fn cli_processes_converge_on_one_object_publication() {
    let d = tempdir().unwrap();
    let mut init = forge();
    init.arg("init").current_dir(d.path());
    run(&mut init);

    let dir = d.path().to_str().unwrap();
    let cap_path = d.path().join(".forge/keys/root.cap");
    let cap = cap_path.to_str().unwrap();

    let payload = vec![0x5a; PAYLOAD_BYTES];
    let payload_file = write_file(d.path(), "same-payload.bin", &payload);
    let expected = blob_id(&payload).hex();
    let sessions = (0..SAME_WRITERS)
        .map(|_| open_session(dir, cap))
        .collect::<Vec<_>>();

    let outputs = race_writes(dir, cap, &sessions, &payload_file, "same");
    assert_write_oid(&outputs, &expected);
    assert_eq!(
        count_named_files(&d.path().join(".forge/objects"), &expected),
        1,
        "identical object bytes must have one canonical durable name"
    );
    fsck_full(dir, cap);

    let a = vec![0x41; PAYLOAD_BYTES];
    let b = vec![0x42; PAYLOAD_BYTES];
    let a_file = write_file(d.path(), "a-payload.bin", &a);
    let b_file = write_file(d.path(), "b-payload.bin", &b);
    let a_ns = open_session(dir, cap);
    let b_ns = open_session(dir, cap);
    let barrier = Arc::new(Barrier::new(2));

    let left = {
        let barrier = Arc::clone(&barrier);
        let dir = dir.to_string();
        let cap = cap.to_string();
        let ns = a_ns.clone();
        std::thread::spawn(move || {
            barrier.wait();
            write_command(&dir, &cap, &ns, "/different-a.bin", &a_file)
                .output()
                .expect("spawn left forge write")
        })
    };
    let right = {
        let barrier = Arc::clone(&barrier);
        let dir = dir.to_string();
        let cap = cap.to_string();
        let ns = b_ns.clone();
        std::thread::spawn(move || {
            barrier.wait();
            write_command(&dir, &cap, &ns, "/different-b.bin", &b_file)
                .output()
                .expect("spawn right forge write")
        })
    };

    let left = left.join().unwrap();
    let right = right.join().unwrap();
    assert_success(&left);
    assert_success(&right);
    assert_eq!(
        String::from_utf8_lossy(&left.stdout).trim(),
        blob_id(&a).hex()
    );
    assert_eq!(
        String::from_utf8_lossy(&right.stdout).trim(),
        blob_id(&b).hex()
    );
    assert_eq!(read_bytes(dir, cap, &a_ns, "/different-a.bin"), a);
    assert_eq!(read_bytes(dir, cap, &b_ns, "/different-b.bin"), b);
    fsck_full(dir, cap);
}
