use crate::Forge;
use forge_protocol::{read_frame, write_frame, Request, Response};
use forge_types::{Error, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use std::net::SocketAddr;
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::thread;

pub fn serve(forge: Arc<Forge>, http: bool) -> Result<()> {
    let root = forge.root().to_path_buf();
    let lock = root.join("LOCK");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .map_err(|_| Error::Busy("LOCK already held".into()))?;
    let sock_path = root.join("forge.sock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    if http {
        let f = forge.clone();
        thread::spawn(move || {
            if let Err(e) = serve_http(f, "127.0.0.1:4077") {
                eprintln!("forge http: {e}");
            }
        });
    }

    let result = accept_loop(forge, listener);
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&lock);
    result
}

fn accept_loop(forge: Arc<Forge>, listener: UnixListener) -> Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let f = forge.clone();
        thread::spawn(move || {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut w = BufWriter::new(stream);
            loop {
                let buf = match read_frame(&mut r) {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let req: Request = match serde_json::from_slice(&buf) {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = write_frame(
                            &mut w,
                            &Response::err(0, &Error::Invalid(e.to_string())),
                        );
                        continue;
                    }
                };
                let resp = dispatch(&f, req);
                if write_frame(&mut w, &resp).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

fn serve_http(forge: Arc<Forge>, addr: &str) -> Result<()> {
    let server = tiny_http::Server::http(addr.parse::<SocketAddr>().map_err(|e| Error::Invalid(e.to_string()))?)
        .map_err(|e| Error::Io(e.to_string()))?;
    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let mut body = Vec::new();
        let _ = std::io::Read::read_to_end(req.as_reader(), &mut body);
        let cap = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .map(|h| h.value.as_str().trim_start_matches("Bearer ").to_string())
            .unwrap_or_default();
        let op = url.trim_start_matches("/v1/").trim_matches('/').to_string();
        let val: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let envelope = Request {
            v: 1,
            id: 1,
            op,
            cap,
            body: val,
        };
        let resp = dispatch(&forge, envelope);
        let payload = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
        let status = if resp.ok { 200 } else { 400 };
        let r = tiny_http::Response::from_data(payload).with_status_code(status);
        let _ = req.respond(r);
    }
    Ok(())
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
            let id = forge.write(&cap, s(&req.body, "ns")?, s(&req.body, "path")?, &data, false)?;
            Ok(json!({ "oid": id.hex() }))
        }
        "ns.read" => {
            let data = forge.read(&cap, s(&req.body, "ns")?, s(&req.body, "path")?)?;
            Ok(json!({ "hex": forge_types::hex_encode(&data) }))
        }
        "ns.ls" => {
            let ents = forge.ls(&cap, s(&req.body, "ns")?, req.body.get("path").and_then(|v| v.as_str()).unwrap_or("/"))?;
            Ok(json!(ents
                .into_iter()
                .map(|(n, k, id, x)| json!({"name": n, "kind": k, "id": id, "exec": x}))
                .collect::<Vec<_>>()))
        }
        "ns.checkin" => {
            let r = forge.checkin(
                &cap,
                s(&req.body, "ns")?,
                req.body.get("mount").and_then(|v| v.as_str()).unwrap_or("/"),
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
                req.body.get("rw").and_then(|v| v.as_bool()).unwrap_or(false),
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


