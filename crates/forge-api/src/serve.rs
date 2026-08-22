use crate::Forge;
use forge_protocol::{read_frame, write_frame, Request, Response};
use forge_types::{Error, Result};
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
use std::time::Duration;

const MAX_HTTP_BODY: usize = 1024 * 1024;
const MAX_UNIX_WORKERS: usize = 64;
const MAX_PENDING_UNIX: usize = 256;
const UNIX_IO_TIMEOUT: Duration = Duration::from_secs(30);

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
        let f = forge.clone();
        thread::Builder::new()
            .name("forge-http".into())
            .spawn(move || {
                if let Err(e) = serve_http(f, "127.0.0.1:4077") {
                    eprintln!("forge http: {e}");
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

fn worker_count() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().saturating_mul(4).clamp(8, MAX_UNIX_WORKERS))
        .unwrap_or(16)
}

fn accept_loop(forge: Arc<Forge>, listener: UnixListener) -> Result<()> {
    // Fixed workers + bounded admission queue: a connection flood consumes a
    // bounded amount of memory and threads instead of spawning without limit.
    let (tx, rx) = sync_channel::<UnixStream>(MAX_PENDING_UNIX);
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..worker_count() {
        let f = forge.clone();
        let rx = rx.clone();
        thread::Builder::new()
            .name(format!("forge-unix-{i}"))
            .spawn(move || loop {
                let stream = {
                    let guard = match rx.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    match guard.recv() {
                        Ok(stream) => stream,
                        Err(_) => return,
                    }
                };
                handle_unix(&f, stream);
            })
            .map_err(|e| Error::Internal(format!("spawn unix worker: {e}")))?;
    }

    for stream in listener.incoming() {
        let stream = stream?;
        match tx.try_send(stream) {
            Ok(()) => {}
            Err(TrySendError::Full(stream)) => {
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

fn handle_unix(forge: &Forge, stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(UNIX_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(UNIX_IO_TIMEOUT));
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut r = BufReader::new(reader_stream);
    let mut w = BufWriter::new(stream);
    while let Ok(buf) = read_frame(&mut r) {
        let req: Request = match serde_json::from_slice(&buf) {
            Ok(x) => x,
            Err(e) => {
                let _ = write_frame(&mut w, &Response::err(0, &Error::Invalid(e.to_string())));
                continue;
            }
        };
        let resp = dispatch(forge, req);
        if write_frame(&mut w, &resp).is_err() {
            break;
        }
    }
}

fn serve_http(forge: Arc<Forge>, addr: &str) -> Result<()> {
    let server = tiny_http::Server::http(
        addr.parse::<SocketAddr>()
            .map_err(|e| Error::Invalid(e.to_string()))?,
    )
    .map_err(|e| Error::Io(e.to_string()))?;

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
            let mut limited = req.as_reader().take((MAX_HTTP_BODY + 1) as u64);
            limited.read_to_end(&mut body)?;
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

fn http_status(resp: &Response) -> u16 {
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

pub fn dispatch(forge: &Forge, req: Request) -> Response {
    match dispatch_inner(forge, &req) {
        Ok(v) => Response::ok(req.id, v),
        Err(e) => Response::err(req.id, &e),
    }
}

fn s<'a>(body: &'a Value, k: &str) -> Result<&'a str> {
    body.get(k)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid(format!("missing {k}")))
}

fn dispatch_inner(forge: &Forge, req: &Request) -> Result<Value> {
    if req.v != 1 {
        return Err(Error::Invalid(format!(
            "unsupported protocol version {}",
            req.v
        )));
    }
    let cap = forge.load_cap(&req.cap)?;
    match req.op.as_str() {
        "session.open" => {
            let ns = forge.session_open(&cap, s(&req.body, "from")?)?;
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
            let ents = forge.ls(
                &cap,
                s(&req.body, "ns")?,
                req.body.get("path").and_then(|v| v.as_str()).unwrap_or("/"),
            )?;
            Ok(json!(ents
                .into_iter()
                .map(|(n, k, id, x)| json!({"name": n, "kind": k, "id": id, "exec": x}))
                .collect::<Vec<_>>()))
        }
        "ns.checkin" => {
            let r = forge.checkin(
                &cap,
                s(&req.body, "ns")?,
                req.body
                    .get("mount")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/"),
                req.body.get("msg").and_then(|v| v.as_str()).unwrap_or(""),
            )?;
            Ok(json!(format!("{r:?}")))
        }
        "ns.mount" => {
            forge.mount(
                &cap,
                s(&req.body, "ns")?,
                s(&req.body, "path")?,
                s(&req.body, "spec")?,
                req.body
                    .get("rw")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            )?;
            Ok(json!({"ok": true}))
        }
        "refs" => {
            let refs = forge.refs(&cap)?;
            Ok(json!(refs
                .into_iter()
                .map(|r| json!({"name": r.name, "oid": r.oid.hex(), "kind": r.kind}))
                .collect::<Vec<_>>()))
        }
        "seal" => {
            let oid = forge.seal(&cap, s(&req.body, "ref")?, s(&req.body, "tag")?)?;
            Ok(json!({"oid": oid.hex()}))
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
