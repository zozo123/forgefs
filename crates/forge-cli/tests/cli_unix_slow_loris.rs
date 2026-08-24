//! Pre-authentication slow loris on the unix socket (I13/I14: authority is
//! explicit, so nothing an *unauthenticated* peer does may cost the service its
//! ability to answer capability holders).
//!
//! `handle_unix` used to set ONE 30s read timeout for the whole connection, and
//! the framing layer only handed back COMPLETE frames. A peer could therefore
//! write the 4-byte length prefix -- or even two bytes of it -- and then go
//! silent, pinning a worker thread for the full 30s. The capability lives
//! inside the frame body, so this needed no credential at all, and
//! `worker_count()` such peers pinned the entire pool. Denial was bounded at
//! roughly ceil(N/worker_count) * 30s rather than permanent, but a request that
//! should take milliseconds took half a minute.
//!
//! The fix is a two-phase deadline: a long wait for the FIRST byte of a frame
//! (an idle connection is legitimate) and a short per-read deadline once any
//! byte has arrived. Because SO_RCVTIMEO is per read syscall, that short
//! deadline reaps a STALLED sender without penalising a merely SLOW one --
//! `slow_but_progressing_sender_is_not_reaped` is the test that pins that
//! distinction down.
//!
//! Note on FIN vs RST: this test never closes the attacking sockets. Dropping
//! them would send FIN, which the server reads as a clean EOF and handles
//! instantly -- the exact opposite of the condition under test. A slow loris
//! holds the connection open and simply stops sending.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{tempdir, TempDir};

/// The legitimate request must come back well inside this. On the fixed server
/// it takes about as long as one committed-frame deadline (~3s); on the unfixed
/// server it takes the full 30s idle timeout. The margin is deliberately huge
/// in both directions so a loaded CI box cannot flip the verdict.
const FAST_ENOUGH: Duration = Duration::from_secs(15);

/// Longer than the committed-frame deadline, comfortably shorter than the idle
/// deadline: an idle connection must survive this.
const IDLE_GAP: Duration = Duration::from_secs(6);

/// Each pause inside a single frame stays well under the committed-frame
/// deadline, while their sum exceeds it.
const PROGRESS_GAP: Duration = Duration::from_millis(1500);

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

/// Returns the tempdir, the daemon guard, the socket path and the root cap.
fn start_daemon() -> (TempDir, Daemon, PathBuf, String) {
    let temp = tempdir().expect("tempdir");
    let out = forge()
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("spawn forge init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cap = std::fs::read_to_string(temp.path().join(".forge/keys/root.cap"))
        .expect("read root cap")
        .trim()
        .to_string();
    let socket = temp.path().join(".forge/forge.sock");

    let child = forge()
        .args([
            "--dir",
            temp.path().to_str().expect("utf8 tempdir"),
            "serve",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forge serve");
    let mut daemon = Daemon(child);
    wait_for_server(&socket, &mut daemon);
    (temp, daemon, socket, cap)
}

fn wait_for_server(path: &Path, daemon: &mut Daemon) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        if let Some(status) = daemon.0.try_wait().expect("poll daemon") {
            panic!("daemon exited before serving: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon never accepted connections on {}", path.display());
}

fn refs_frame(cap: &str, id: u64) -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({
        "v": 1, "id": id, "op": "refs", "cap": cap, "body": {}
    }))
    .expect("encode request");
    let mut framed = (body.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&body);
    framed
}

/// Read one length-prefixed response frame and assert the server said ok.
fn expect_ok_response(s: &mut UnixStream, id: u64) {
    let mut lenb = [0u8; 4];
    s.read_exact(&mut lenb)
        .unwrap_or_else(|e| panic!("no response length for id {id}: {e}"));
    let n = u32::from_be_bytes(lenb) as usize;
    assert!(n > 0 && n < 1 << 20, "implausible response length {n}");
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("truncated response for id {id}: {e}"));
    let v: serde_json::Value = serde_json::from_slice(&buf).expect("response is JSON");
    assert_eq!(v["id"], serde_json::json!(id), "response id mismatch: {v}");
    assert_eq!(v["ok"], serde_json::json!(true), "server refused: {v}");
}

/// A loris: open the connection, emit `header_bytes` of the 4-byte length
/// prefix, never send anything else, never close. Returned so the caller keeps
/// it alive -- dropping it would send FIN and defeat the point.
fn stall(socket: &Path, header_bytes: usize) -> UnixStream {
    let mut s = UnixStream::connect(socket).expect("loris connect");
    // Claim a 4 KiB body that will never arrive.
    let prefix = 4096u32.to_be_bytes();
    s.write_all(&prefix[..header_bytes]).expect("loris write");
    s.flush().expect("loris flush");
    s
}

#[test]
fn stalled_unauthenticated_peers_do_not_pin_the_worker_pool() {
    let (_temp, _daemon, socket, cap) = start_daemon();

    // Saturate exactly the pool this machine runs, whatever its core count.
    let workers = forge_api::unix_worker_count();
    let mut lorises = Vec::with_capacity(workers);
    for i in 0..workers {
        // Alternate a complete length prefix with a torn one: both leave the
        // worker blocked mid-frame, and the torn one is what makes splitting
        // the header read necessary.
        lorises.push(stall(&socket, if i % 2 == 0 { 4 } else { 2 }));
    }
    // Give every worker time to pick up its loris.
    thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    let mut client = UnixStream::connect(&socket).expect("legitimate connect");
    client
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("set client read timeout");
    client.write_all(&refs_frame(&cap, 1)).expect("send refs");
    expect_ok_response(&mut client, 1);
    let waited = started.elapsed();

    // Keep the attackers alive until after the measurement.
    drop(lorises);

    assert!(
        waited < FAST_ENOUGH,
        "a legitimate refs call took {waited:?} while {workers} unauthenticated \
         peers sat silent mid-frame; the whole worker pool was pinned for the \
         idle timeout instead of the much shorter committed-frame deadline"
    );
}

#[test]
fn legitimate_traffic_survives_the_two_phase_deadline() {
    let (_temp, _daemon, socket, cap) = start_daemon();
    let mut s = UnixStream::connect(&socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .expect("set read timeout");

    // One ordinary request/response.
    s.write_all(&refs_frame(&cap, 1)).expect("write frame 1");
    expect_ok_response(&mut s, 1);

    // Several frames on one connection.
    for id in 2..=4 {
        s.write_all(&refs_frame(&cap, id)).expect("write frame");
        expect_ok_response(&mut s, id);
    }

    // Idle between requests for longer than the committed-frame deadline. The
    // connection has committed to nothing, so the LONG deadline applies and it
    // must still be alive.
    thread::sleep(IDLE_GAP);
    s.write_all(&refs_frame(&cap, 5)).expect("write after idle");
    expect_ok_response(&mut s, 5);
}

#[test]
fn slow_but_progressing_sender_is_not_reaped() {
    let (_temp, _daemon, socket, cap) = start_daemon();
    let mut s = UnixStream::connect(&socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .expect("set read timeout");

    let framed = refs_frame(&cap, 9);
    let (prefix, body) = framed.split_at(4);
    let chunk = body.len() / 3 + 1;

    // Header, then the body in thirds. Total time spent inside the frame
    // exceeds the committed-frame deadline, but no single gap does -- and
    // SO_RCVTIMEO is per read syscall, so this client must be served.
    s.write_all(prefix).expect("write prefix");
    s.flush().expect("flush prefix");
    for part in body.chunks(chunk) {
        thread::sleep(PROGRESS_GAP);
        s.write_all(part).expect("write body chunk");
        s.flush().expect("flush body chunk");
    }
    assert!(
        PROGRESS_GAP * 3 > Duration::from_secs(3),
        "the pauses no longer add up to more than the committed-frame deadline, \
         so this test would pass even with a per-frame budget"
    );
    expect_ok_response(&mut s, 9);
}
