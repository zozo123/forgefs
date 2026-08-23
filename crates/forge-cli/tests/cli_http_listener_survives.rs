//! One unauthenticated aborted request must not kill `serve --http`.
//!
//! `serve_http` read the request body with `read_to_end(..)?`, and that `?`
//! propagated out of the `for req in server.incoming_requests()` loop -- so a
//! client that promised a body and then sent RST made the whole listener exit.
//! Every later connect got ECONNREFUSED, permanently, and the unix daemon kept
//! running healthy so nothing reported that `--http` now meant no HTTP. The
//! body is read before the Authorization header is parsed, so no capability was
//! needed either.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const ADDR: &str = "127.0.0.1:47311";
const ADDR_ALT: &str = "127.0.0.1:47312";

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_listener(deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if TcpStream::connect(ADDR).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A well-formed POST. Returns the status line, or None if the port is dead.
fn post(cap: &str) -> Option<String> {
    let mut s = TcpStream::connect(ADDR).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let body = "{}";
    let req = format!(
        "POST /v1/refs HTTP/1.1\r\nHost: l\r\nConnection: close\r\n\
         Authorization: Bearer {cap}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.lines().next().map(|l| l.to_string())
}

/// Abort a connection with a TCP RST rather than a graceful FIN.
///
/// This distinction is the whole test. Dropping the socket normally sends FIN,
/// and the server's `read_to_end` then sees a clean EOF and returns Ok with a
/// short body -- so the bug does not reproduce. Forcing SO_LINGER=0 makes the
/// close emit RST, the server's read fails with ECONNRESET, and that is the
/// error that used to escape the accept loop and kill the listener.
fn abort_with_rst(stream: TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: fd is owned by `stream` for the duration of the call, and the
    // option value is a correctly-sized libc::linger.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const libc::linger as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(
        rc, 0,
        "could not set SO_LINGER; the test cannot force an RST"
    );
    drop(stream);
}

#[test]
fn aborted_request_body_does_not_kill_the_http_listener() {
    let d = tempdir().unwrap();
    let dir = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&dir)
        .output()
        .unwrap()
        .status
        .success());
    let cap_path = dir.join(".forge/keys/root.cap");
    let cap = std::fs::read_to_string(&cap_path)
        .unwrap()
        .trim()
        .to_string();

    let child = forge()
        .arg("--dir")
        .arg(&dir)
        .arg("--cap")
        .arg(&cap_path)
        .args(["serve", "--http"])
        .env("FORGE_HTTP_ADDR", ADDR)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forge serve");
    let _daemon = Daemon(child);

    assert!(
        wait_for_listener(Duration::from_secs(20)),
        "daemon never bound {ADDR}"
    );

    // Healthy to begin with.
    let before = post(&cap).expect("listener answered before the abort");
    assert!(before.contains("200"), "unexpected status: {before}");

    // The kill: promise a body, send a fraction of it, then drop the connection
    // hard. No capability required -- the body is read first.
    {
        let mut s = TcpStream::connect(ADDR).unwrap();
        let req = "POST /v1/refs HTTP/1.1\r\nHost: l\r\nContent-Length: 100000\r\n\r\n{\"a\":1}";
        s.write_all(req.as_bytes()).unwrap();
        abort_with_rst(s);
    }
    std::thread::sleep(Duration::from_millis(300));

    // The listener must still be there.
    let after = post(&cap).expect("listener died after one aborted request body");
    assert!(
        after.contains("200"),
        "unexpected status after abort: {after}"
    );

    // And still there after several more.
    for _ in 0..5 {
        let mut s = TcpStream::connect(ADDR).unwrap();
        let _ = s
            .write_all(b"POST /v1/refs HTTP/1.1\r\nHost: l\r\nContent-Length: 100000\r\n\r\nshort");
        abort_with_rst(s);
    }
    std::thread::sleep(Duration::from_millis(300));
    let finally = post(&cap).expect("listener died after repeated aborted bodies");
    assert!(finally.contains("200"), "unexpected status: {finally}");
}

#[test]
fn http_bind_failure_is_reported_at_startup_not_silently_ignored() {
    // Hold the port so the daemon cannot have it.
    let squatter = std::net::TcpListener::bind(ADDR_ALT).expect("bind squatter");

    let d = tempdir().unwrap();
    let dir = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&dir)
        .output()
        .unwrap()
        .status
        .success());

    let out = forge()
        .arg("--dir")
        .arg(&dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"))
        .args(["serve", "--http"])
        .env("FORGE_HTTP_ADDR", ADDR_ALT)
        .output()
        .expect("spawn forge serve");

    drop(squatter);
    assert!(
        !out.status.success(),
        "serve --http succeeded despite being unable to bind; \
         --http must not silently mean no HTTP"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot listen on"),
        "bind failure not reported: {stderr}"
    );
}
