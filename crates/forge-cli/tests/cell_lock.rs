use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn run_ok(cmd: &mut Command) -> String {
    let output = cmd.output().expect("spawn forge");
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn spawn_serve(dir: &str) -> Child {
    forge()
        .args(["--dir", dir, "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forge serve")
}

fn wait_for_server(path: &std::path::Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll daemon") {
            panic!("daemon exited before serving: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon did not accept socket connections: {}", path.display());
}

#[test]
fn daemon_and_direct_clients_cannot_split_brain() {
    let temp = tempdir().unwrap();
    run_ok(forge().arg("init").current_dir(temp.path()));
    let dir = temp.path().to_str().unwrap();
    let cap = temp.path().join(".forge/keys/root.cap");
    let cap = cap.to_str().unwrap();
    let socket = temp.path().join(".forge/forge.sock");

    let mut daemon = spawn_serve(dir);
    wait_for_server(&socket, &mut daemon);

    let direct = forge()
        .args(["--dir", dir, "--cap", cap, "refs"])
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&direct.stderr).contains("daemon"),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(
        socket.exists(),
        "direct open must never unlink a live daemon socket"
    );

    let second = forge().args(["--dir", dir, "serve"]).output().unwrap();
    assert_eq!(second.status.code(), Some(3));

    daemon.kill().unwrap();
    daemon.wait().unwrap();

    // Kernel ownership, not the stale socket/LOCK pathname, decides authority.
    run_ok(forge().args(["--dir", dir, "--cap", cap, "refs"]));
    assert!(temp.path().join(".forge/LOCK").exists());
    assert!(
        UnixStream::connect(&socket).is_err(),
        "SIGKILL should leave at most a stale, non-serving socket pathname"
    );

    let mut restarted = spawn_serve(dir);
    wait_for_server(&socket, &mut restarted);
    restarted.kill().unwrap();
    restarted.wait().unwrap();
}
