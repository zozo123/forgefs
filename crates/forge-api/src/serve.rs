use crate::Forge;
use forge_protocol::{read_frame_body, read_frame_len_after, write_frame, Request, Response};
use forge_types::{CasResult, Error, Result};
use serde_json::{json, Value};
use std::io::{BufReader, BufWriter, Read};
use std::net::{Shutdown, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    mpsc::{sync_channel, TrySendError},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

const MAX_HTTP_BODY: usize = 1024 * 1024;
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:4077";

/// Listen address for `serve --http`. Two forges with `--http` on one host
/// cannot share the default port, and the bind failure is now reported at
/// startup rather than silently ignored, so give operators a way out.
fn http_addr() -> String {
    std::env::var("FORGE_HTTP_ADDR").unwrap_or_else(|_| DEFAULT_HTTP_ADDR.to_string())
}
const MAX_UNIX_WORKERS: usize = 64;
const MAX_PENDING_UNIX: usize = 256;

/// How long a worker will wait for the FIRST byte of a frame.
///
/// An idle connection is legitimate: a client may hold one open between
/// requests. Nothing is committed and no buffer is reserved while we wait here,
/// so this stays generous.
const UNIX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a worker will wait for the NEXT byte once a frame has begun.
///
/// `SO_RCVTIMEO` is a per-`read`-syscall deadline, not a per-frame budget, so a
/// slow-but-progressing sender resets it on every chunk it delivers and is
/// never penalised for being slow; only a sender that has gone silent
/// mid-frame is reaped. That distinction is the entire point of having two
/// numbers here -- do NOT collapse them back into one. The capability is
/// parsed only after a whole frame arrives, so a single long deadline lets
/// unauthenticated peers each pin a worker for the full duration, and
/// `worker_count()` of them pin the whole pool.
const UNIX_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a worker will spend on a single blocking write.
const UNIX_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an accepted connection may sit in the admission queue before a
/// worker discards it instead of serving it.
///
/// `MAX_PENDING_UNIX` bounds memory but imposed no time bound: a queued
/// connection waited for a free worker however long that took. Reject late
/// instead of never, so the client gets `busy` and can retry.
const UNIX_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

pub fn serve(forge: Arc<Forge>, http: bool) -> Result<()> {
    if !forge.has_exclusive_cell_lock() {
        return Err(Error::Busy(
            "forge serve requires Forge::open_for_serve exclusive cell ownership".into(),
        ));
    }
    let root = forge.root().to_path_buf();
    let sock_path = root.join("forge.sock");
    // Exclusive cell ownership proves no live direct client or daemon exists;
    // only now is it safe to remove a stale socket pathname.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    if http {
        // Bind HERE, on the caller's thread, so a failure to take the port is a
        // startup error the operator sees immediately. Binding inside the
        // spawned thread meant `--http` could be accepted while the listener
        // never existed -- a second forge on the same host lost the bind, kept
        // running, and port 4077 went on answering for the FIRST forge.
        let server = http_listener(&http_addr())?;
        let f = forge.clone();
        thread::Builder::new()
            .name("forge-http".into())
            .spawn(move || {
                // The accept loop no longer aborts on a bad request, so reaching
                // here means something structural. Say so; do not leave `--http`
                // silently meaning "no HTTP".
                if let Err(e) = http_accept_loop(f, server) {
                    eprintln!("forge http: listener stopped: {e}");
                }
            })
            .map_err(|e| Error::Internal(format!("spawn http thread: {e}")))?;
    }

    // Forge owns the exclusive LOCK descriptor for the service lifetime.
    // Keep the LOCK pathname persistent: unlinking it after unlock can split the
    // rendezvous inode from a concurrently opening client.
    let result = accept_loop(forge, listener);
    let _ = std::fs::remove_file(&sock_path);
    result
}

/// Number of unix-socket worker threads this build will run.
///
/// Exposed so a test can saturate exactly the pool that exists on the machine
/// it happens to be running on, instead of guessing.
pub fn unix_worker_count() -> usize {
    worker_count()
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().saturating_mul(4).clamp(8, MAX_UNIX_WORKERS))
        .unwrap_or(16)
}

fn accept_loop(forge: Arc<Forge>, listener: UnixListener) -> Result<()> {
    // Fixed workers + bounded admission queue: a connection flood consumes a
    // bounded amount of memory and threads instead of spawning without limit.
    // Queued connections carry their accept time so a worker can tell how long
    // they have been waiting and give up on stale ones.
    let (tx, rx) = sync_channel::<(UnixStream, Instant)>(MAX_PENDING_UNIX);
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..worker_count() {
        let f = forge.clone();
        let rx = rx.clone();
        thread::Builder::new()
            .name(format!("forge-unix-{i}"))
            .spawn(move || loop {
                let (stream, queued_at) = {
                    let guard = match rx.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    match guard.recv() {
                        Ok(item) => item,
                        Err(_) => return,
                    }
                };
                if queued_at.elapsed() > UNIX_ADMISSION_TIMEOUT {
                    reject_stale_admission(stream);
                    continue;
                }
                handle_unix(&f, stream);
            })
            .map_err(|e| Error::Internal(format!("spawn unix worker: {e}")))?;
    }

    for stream in listener.incoming() {
        let stream = stream?;
        match tx.try_send((stream, Instant::now())) {
            Ok(()) => {}
            Err(TrySendError::Full((stream, _))) => {
                // Fast overload rejection is safer than queueing unbounded work.
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(Error::Internal("unix worker pool disconnected".into()));
            }
        }
    }
    Ok(())
}

/// Tell a connection that waited too long in the admission queue to retry,
/// rather than serving a request whose caller has very likely given up.
fn reject_stale_admission(stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(UNIX_FRAME_TIMEOUT));
    let mut w = BufWriter::new(&stream);
    let _ = write_frame(
        &mut w,
        &Response::err(
            0,
            &Error::Busy("server saturated; connection queued too long".into()),
        ),
    );
    drop(w);
    let _ = stream.shutdown(Shutdown::Both);
}

fn handle_unix(forge: &Forge, stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(UNIX_WRITE_TIMEOUT));
    // Two extra handles on the same socket: `deadline` exists only to retune
    // SO_RCVTIMEO mid-frame, because `stream` itself is moved into the
    // BufWriter and `reader_stream` is buried inside the BufReader.
    let Ok(deadline) = stream.try_clone() else {
        return;
    };
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(reader_stream);
    let mut w = BufWriter::new(stream);
    loop {
        // Phase 1: the connection is idle and has committed to nothing. Wait
        // generously for the first byte of the next frame.
        if deadline.set_read_timeout(Some(UNIX_IDLE_TIMEOUT)).is_err() {
            return;
        }
        let mut first = [0u8; 1];
        if r.read_exact(&mut first).is_err() {
            return;
        }
        // Phase 2: a frame is now in flight. Tighten the per-read deadline
        // BEFORE reading byte two of the length prefix, because sending one
        // byte and stopping is the cheapest way to pin this worker. See
        // UNIX_FRAME_TIMEOUT: this reaps stalled senders, not slow ones.
        if deadline.set_read_timeout(Some(UNIX_FRAME_TIMEOUT)).is_err() {
            return;
        }
        let Ok(n) = read_frame_len_after(&mut r, first[0]) else {
            return;
        };
        let Ok(buf) = read_frame_body(&mut r, n) else {
            return;
        };
        let req: Request = match serde_json::from_slice(&buf) {
            Ok(x) => x,
            Err(e) => {
                let _ = write_frame(&mut w, &Response::err(0, &Error::Invalid(e.to_string())));
                continue;
            }
        };
        let resp = dispatch(forge, req);
        if write_frame(&mut w, &resp).is_err() {
            return;
        }
    }
}

fn http_listener(addr: &str) -> Result<tiny_http::Server> {
    tiny_http::Server::http(
        addr.parse::<SocketAddr>()
            .map_err(|e| Error::Invalid(e.to_string()))?,
    )
    .map_err(|e| Error::Io(format!("cannot listen on {addr}: {e}")))
}

fn http_accept_loop(forge: Arc<Forge>, server: tiny_http::Server) -> Result<()> {
    for mut req in server.incoming_requests() {
        if req.method() != &tiny_http::Method::Post {
            let r = tiny_http::Response::from_string("method not allowed").with_status_code(405);
            let _ = req.respond(r);
            continue;
        }

        let url = req.url().to_string();
        let Some(op) = url
            .strip_prefix("/v1/")
            .map(|s| s.trim_matches('/').to_string())
        else {
            let r = tiny_http::Response::from_string("not found").with_status_code(404);
            let _ = req.respond(r);
            continue;
        };
        if op.is_empty() {
            let r = tiny_http::Response::from_string("not found").with_status_code(404);
            let _ = req.respond(r);
            continue;
        }

        let mut body = Vec::new();
        {
            // A client that promises a body and then disappears is one bad
            // request, not a reason to stop serving. Propagating this error with
            // `?` returned it out of the accept loop, so serve_http exited and
            // the listener socket closed: every later connect got
            // ECONNREFUSED, permanently, from one unauthenticated aborted
            // request. The body is read before the capability header is parsed,
            // so no credential was needed either.
            let mut limited = req.as_reader().take((MAX_HTTP_BODY + 1) as u64);
            if let Err(error) = limited.read_to_end(&mut body) {
                let r = tiny_http::Response::from_string(format!(
                    "could not read request body: {error}"
                ))
                .with_status_code(400);
                let _ = req.respond(r);
                continue;
            }
        }
        if body.len() > MAX_HTTP_BODY {
            let r =
                tiny_http::Response::from_string("request body too large").with_status_code(413);
            let _ = req.respond(r);
            continue;
        }

        let cap = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string();
        let val: Value = if body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    let payload = serde_json::to_vec(&Response::err(
                        1,
                        &Error::Invalid(format!("invalid JSON: {e}")),
                    ))
                    .unwrap_or_else(|_| b"{}".to_vec());
                    let r = tiny_http::Response::from_data(payload).with_status_code(400);
                    let _ = req.respond(r);
                    continue;
                }
            }
        };

        let envelope = Request {
            v: 1,
            id: 1,
            op,
            cap,
            body: val,
        };
        let resp = dispatch(&forge, envelope);
        let payload = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
        let r = tiny_http::Response::from_data(payload).with_status_code(http_status(&resp));
        let _ = req.respond(r);
    }
    Ok(())
}

/// The daemon's HTTP status for a response.
///
/// Public so the daemon-ABI conformance test can pin one status per error
/// class instead of restating the mapping. HTTP is lossier than the exit-code
/// table on purpose -- 409 covers `sealed`, `conflict`, `stale_observation`
/// and `invalid_base` alike -- so a client that needs the CLI's classification
/// reads `err.code`, never the status. CLI_ABI.md states this.
pub fn http_status(resp: &Response) -> u16 {
    if resp.ok {
        return 200;
    }
    match resp.err.as_ref().map(|e| e.code.as_str()) {
        Some("denied") => 403,
        Some("not_found") => 404,
        Some("sealed" | "conflict" | "stale_observation" | "invalid_base") => 409,
        Some("busy") => 503,
        Some("invalid") => 400,
        Some("corrupt" | "internal") | None => 500,
        Some(_) => 500,
    }
}

/// Every op the daemon serves, and every request-body field each one accepts.
///
/// This table IS the daemon ABI's argument surface, and it exists so that
/// surface is a projection of the CLI rather than a second, wider one (#332).
/// Each row names a CLI verb and lists exactly the inputs that verb takes;
/// anything else is refused, so a daemon client can ask for precisely what a
/// CLI user can ask for and no more. A field ForgeFS does not know is an
/// error, never a silent default -- `{"mnt": "/x"}` used to be accepted as a
/// checkin of the DEFAULT mount, and `{"attest": true}` used to seal without
/// attesting and answer 200.
///
/// Adding an op or a field here is the deliberate act of widening the daemon
/// ABI. `daemon_abi.rs` pins this table against CLI_ABI.md, so it cannot be
/// widened by accident.
pub const DAEMON_OPS: &[(&str, &[&str])] = &[
    // `forge session open [--from]`
    ("session.open", &["from"]),
    // `forge write --ns <path> --file|--text`; `hex` is the wire form of
    // `--file`, since a daemon client has no path on the server's filesystem.
    ("ns.write", &["ns", "path", "text", "hex"]),
    // `forge read --ns <path>`
    ("ns.read", &["ns", "path"]),
    // `forge ls --ns [path]`
    ("ns.ls", &["ns", "path"]),
    // `forge checkin --ns [--mount] [-m]`
    ("ns.checkin", &["ns", "mount", "msg"]),
    // `forge mount --ns <path> <spec> [--rw]`
    ("ns.mount", &["ns", "path", "spec", "rw"]),
    // `forge refs`
    ("refs", &[]),
    // `forge seal <ref> --tag [--attest]`
    ("seal", &["ref", "tag", "attest"]),
];

pub fn dispatch(forge: &Forge, req: Request) -> Response {
    match dispatch_inner(forge, &req) {
        Ok(v) => Response::ok(req.id, v),
        Err(e) => Response::err(req.id, &e),
    }
}

/// Refuse an op this daemon does not serve, and any field its CLI verb cannot
/// express, before the op runs.
fn check_request_shape(op: &str, body: &Value) -> Result<()> {
    let Some((_, accepted)) = DAEMON_OPS.iter().find(|(name, _)| *name == op) else {
        return Err(Error::Invalid(format!("unknown op {op}")));
    };
    let fields = match body {
        Value::Object(fields) => fields,
        // An absent body is the empty body; anything else is not a request.
        Value::Null => return Ok(()),
        _ => {
            return Err(Error::Invalid(format!(
                "op {op} body must be a JSON object"
            )))
        }
    };
    for key in fields.keys() {
        if !accepted.contains(&key.as_str()) {
            return Err(Error::Invalid(format!(
                "unknown field {key:?} for op {op}; accepted: {}",
                if accepted.is_empty() {
                    "(none)".to_string()
                } else {
                    accepted.join(", ")
                }
            )));
        }
    }
    Ok(())
}

fn s<'a>(body: &'a Value, k: &str) -> Result<&'a str> {
    body.get(k)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid(format!("missing {k}")))
}

/// An optional string field with the CLI default for the same argument.
fn opt<'a>(body: &'a Value, k: &str, default: &'a str) -> Result<&'a str> {
    match body.get(k) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(v)) => Ok(v),
        Some(_) => Err(Error::Invalid(format!("{k} must be a string"))),
    }
}

/// A boolean field standing for a CLI flag: a flag is present or absent, so
/// anything that is not a JSON boolean is an input error rather than `false`.
fn flag(body: &Value, k: &str) -> Result<bool> {
    match body.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
        Some(_) => Err(Error::Invalid(format!("{k} must be a boolean"))),
    }
}

/// A CAS outcome in the CLI's own three-word vocabulary.
///
/// This body used to be `format!("{r:?}")` -- a Rust `Debug` rendering, e.g.
/// `Updated { name: "heads/...", oid: ObjectId(b392...) }`. Nothing documented
/// it, nothing tested it, and any client that parsed it would have pinned
/// ForgeFS to a derived formatter.
fn cas_result(result: CasResult) -> Value {
    match result {
        CasResult::Updated { name, oid } => {
            json!({"result": "updated", "name": name, "oid": oid.hex()})
        }
        CasResult::Forked {
            requested,
            fork,
            ours,
            theirs,
        } => json!({
            "result": "forked",
            "requested": requested,
            "fork": fork,
            "ours": ours.hex(),
            "theirs": theirs.hex(),
        }),
        CasResult::Noop { name, oid } => {
            json!({"result": "noop", "name": name, "oid": oid.hex()})
        }
    }
}

fn dispatch_inner(forge: &Forge, req: &Request) -> Result<Value> {
    if req.v != 1 {
        return Err(Error::Invalid(format!(
            "unsupported protocol version {}",
            req.v
        )));
    }
    let cap = forge.load_cap(&req.cap)?;
    check_request_shape(&req.op, &req.body)?;
    match req.op.as_str() {
        "session.open" => {
            // `forge session open` defaults --from to main; so does this.
            let ns = forge.session_open(&cap, opt(&req.body, "from", "main")?)?;
            Ok(json!({ "ns": ns }))
        }
        "ns.write" => {
            let data = if let Some(hex) = req.body.get("hex").and_then(|v| v.as_str()) {
                forge_types::hex_decode(hex)?
            } else if let Some(t) = req.body.get("text").and_then(|v| v.as_str()) {
                t.as_bytes().to_vec()
            } else {
                return Err(Error::Invalid("write needs text or hex".into()));
            };
            let id = forge.write(
                &cap,
                s(&req.body, "ns")?,
                s(&req.body, "path")?,
                &data,
                false,
            )?;
            Ok(json!({ "oid": id.hex() }))
        }
        "ns.read" => {
            let data = forge.read(&cap, s(&req.body, "ns")?, s(&req.body, "path")?)?;
            Ok(json!({ "hex": forge_types::hex_encode(&data) }))
        }
        "ns.ls" => {
            let ents = forge.ls(&cap, s(&req.body, "ns")?, opt(&req.body, "path", "/")?)?;
            Ok(json!(ents
                .into_iter()
                .map(|(n, k, id, x)| json!({"name": n, "kind": k, "id": id, "exec": x}))
                .collect::<Vec<_>>()))
        }
        "ns.checkin" => {
            // Exactly `forge checkin --ns [--mount] [-m]`, defaults included.
            let r = forge.checkin(
                &cap,
                s(&req.body, "ns")?,
                opt(&req.body, "mount", "/")?,
                opt(&req.body, "msg", "")?,
            )?;
            Ok(cas_result(r))
        }
        "ns.mount" => {
            forge.mount(
                &cap,
                s(&req.body, "ns")?,
                s(&req.body, "path")?,
                s(&req.body, "spec")?,
                flag(&req.body, "rw")?,
            )?;
            Ok(json!({"ok": true}))
        }
        "refs" => {
            // Same rows and the same authority filtering as `forge refs`,
            // including the P/S flags that listing prints.
            let refs = forge.refs(&cap)?;
            Ok(json!(refs
                .into_iter()
                .map(|r| json!({
                    "name": r.name,
                    "oid": r.oid.hex(),
                    "kind": r.kind,
                    "protected": r.protected,
                    "sealed": r.sealed,
                }))
                .collect::<Vec<_>>()))
        }
        "seal" => {
            let tag = s(&req.body, "tag")?;
            let attest = flag(&req.body, "attest")?;
            let oid = forge.seal(&cap, s(&req.body, "ref")?, tag)?;
            // `forge seal --attest` re-reads the durable bytes before it
            // reports success. A daemon client asking for the same thing used
            // to be answered 200 with no attestation performed at all.
            if attest {
                forge.verify_tag(&cap, tag)?;
            }
            Ok(json!({"oid": oid.hex(), "attested": attest}))
        }
        other => Err(Error::Invalid(format!("unknown op {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pool_is_bounded() {
        assert!((8..=MAX_UNIX_WORKERS).contains(&worker_count()));
        assert_eq!(MAX_PENDING_UNIX, 256);
    }

    #[test]
    fn frame_deadline_is_far_shorter_than_the_idle_deadline() {
        // The two-phase deadline is only a defence if the committed-frame
        // deadline is much tighter than the idle one; collapsing them back
        // together restores the pre-auth slow loris.
        assert!(
            UNIX_FRAME_TIMEOUT * 4 < UNIX_IDLE_TIMEOUT,
            "frame deadline {UNIX_FRAME_TIMEOUT:?} is not meaningfully tighter \
             than idle deadline {UNIX_IDLE_TIMEOUT:?}"
        );
        assert!(UNIX_ADMISSION_TIMEOUT < UNIX_IDLE_TIMEOUT);
    }

    #[test]
    fn http_errors_have_semantic_status_codes() {
        assert_eq!(
            http_status(&Response::err(1, &Error::Denied("x".into()))),
            403
        );
        assert_eq!(
            http_status(&Response::err(1, &Error::NotFound("x".into()))),
            404
        );
        assert_eq!(
            http_status(&Response::err(1, &Error::Busy("x".into()))),
            503
        );
        assert_eq!(
            http_status(&Response::err(
                1,
                &Error::StaleObservation {
                    path: "x".into(),
                    expected: "a".into(),
                    found: "b".into(),
                }
            )),
            409
        );
        assert_eq!(
            http_status(&Response::err(1, &Error::Corrupt("x".into()))),
            500
        );
    }
}
