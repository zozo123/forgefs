//! Conformance for the daemon ABI (#332).
//!
//! `forge serve` used to be outside every contract in this repository: no
//! invariant named it, CLI_ABI.md did not describe it, and
//! `scripts/cli-abi-conformance.sh` drives the CLI binary alone -- which has no
//! daemon client to drive. So the daemon's argument surface, its response
//! shapes and its error mapping were all unspecified AND untested, and the
//! first consumer to build against `serve` would have frozen whatever it
//! happened to observe.
//!
//! The resolution is one contract, not two: the daemon is a PROJECTION of the
//! CLI. Every op is a CLI verb, every field is that verb's own argument with
//! that verb's own default, and every error carries the same classification the
//! CLI's exit code carries. This file is that contract's conformance suite, and
//! it is deliberately written so that widening `dispatch` by accident fails
//! here rather than shipping.

use forge_api::{daemon_http_status, dispatch_request, Forge, DAEMON_OPS};
use forge_protocol::{Request, Response};
use forge_types::{Error, ObjectId};
use serde_json::{json, Value};
use std::path::Path;
use tempfile::{tempdir, TempDir};

fn fixture() -> (TempDir, Forge, String) {
    let d = tempdir().unwrap();
    let f = Forge::init(d.path()).unwrap();
    let token = root_token(d.path());
    (d, f, token)
}

fn root_token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".forge/keys/root.cap"))
        .expect("root capability")
        .trim()
        .to_string()
}

fn call(f: &Forge, token: &str, op: &str, body: Value) -> Response {
    dispatch_request(
        f,
        Request {
            v: 1,
            id: 7,
            op: op.into(),
            cap: token.into(),
            body,
        },
    )
}

fn ok(f: &Forge, token: &str, op: &str, body: Value) -> Value {
    let r = call(f, token, op, body);
    assert!(r.ok, "{op} failed: {:?}", r.err);
    let body = r.body.expect("ok response carries a body");
    // Every documented daemon response is a JSON object of named fields, the
    // one exception being `refs`, which is an array of them. A scalar body is
    // the shape of the regression this file exists to prevent: `ns.checkin`
    // used to answer `format!("{r:?}")`, a JSON *string* holding a Rust
    // `Debug` rendering that nothing documented and nothing tested.
    assert!(
        if op == "refs" {
            body.is_array()
        } else {
            body.is_object()
        },
        "{op} answered with {body}, which is not the documented response shape: a scalar body \
         is a leaked internal rendering, not a contract"
    );
    body
}

fn err_code(r: &Response) -> String {
    assert!(!r.ok, "expected a refusal, got {:?}", r.body);
    r.err.as_ref().expect("refusal carries err").code.clone()
}

fn err_msg(r: &Response) -> String {
    r.err.as_ref().expect("refusal carries err").msg.clone()
}

fn open_session(f: &Forge, token: &str) -> String {
    ok(f, token, "session.open", json!({"from": "main"}))["ns"]
        .as_str()
        .expect("ns is a string")
        .to_string()
}

/// A body carrying every required field of `op`, so the only reason a request
/// built from it can fail is the thing a test deliberately added to it.
fn minimal_body(op: &str, ns: &str) -> Value {
    match op {
        "session.open" => json!({"from": "main"}),
        "ns.write" => json!({"ns": ns, "path": "/probe.txt", "text": "probe"}),
        "ns.read" => json!({"ns": ns, "path": "/probe.txt"}),
        "ns.ls" => json!({"ns": ns}),
        "ns.checkin" => json!({"ns": ns, "msg": "probe"}),
        "ns.mv" => json!({"ns": ns, "from": "/probe.txt", "to": "/probe-moved.txt"}),
        "ns.mount" => json!({"ns": ns, "path": "/probe", "spec": "ref:main"}),
        "refs" => json!({}),
        "seal" => json!({"ref": "main", "tag": "probe"}),
        other => panic!("no minimal body for op {other}; add one when you add the op"),
    }
}

/// CLI_ABI.md's daemon table, restated here by hand.
///
/// Two independent statements of the same contract: `DAEMON_OPS` is what the
/// code serves, this is what the document promises. Adding an op or a field to
/// `dispatch` without writing it down fails right here.
const DOCUMENTED: &[(&str, &[&str])] = &[
    ("session.open", &["from"]),
    ("ns.write", &["ns", "path", "text", "hex"]),
    ("ns.read", &["ns", "path"]),
    ("ns.ls", &["ns", "path"]),
    ("ns.mv", &["ns", "from", "to", "expect_oid"]),
    ("ns.checkin", &["ns", "mount", "msg"]),
    ("ns.mount", &["ns", "path", "spec", "rw"]),
    ("refs", &[]),
    ("seal", &["ref", "tag", "attest"]),
];

#[test]
fn daemon_serves_exactly_the_documented_projection_of_the_cli() {
    assert_eq!(
        DAEMON_OPS.len(),
        DOCUMENTED.len(),
        "daemon op set changed: served={:?} documented={:?}",
        DAEMON_OPS.iter().map(|(op, _)| *op).collect::<Vec<_>>(),
        DOCUMENTED.iter().map(|(op, _)| *op).collect::<Vec<_>>()
    );
    for (op, documented) in DOCUMENTED {
        let (_, served) = DAEMON_OPS
            .iter()
            .find(|(name, _)| name == op)
            .unwrap_or_else(|| panic!("CLI_ABI.md documents daemon op {op}, dispatch has no arm"));
        assert_eq!(
            served, documented,
            "daemon op {op} accepts {served:?} but CLI_ABI.md documents {documented:?}"
        );
    }
}

#[test]
fn an_op_the_daemon_does_not_serve_is_refused_as_input() {
    let (_d, f, token) = fixture();
    // Every one of these is a real CLI verb. The daemon is a strict subset of
    // the CLI on purpose, and asking for the rest is an input error, never a
    // partially-implemented success.
    for op in [
        "import",
        "merge",
        "branch",
        "grant",
        "gc",
        "abandon",
        "fsck",
        "verify",
        "export",
        "log",
        "show",
        "stats",
        "inbox",
        "landmark",
        "ns.checkout",
    ] {
        let r = call(&f, &token, op, json!({}));
        assert_eq!(err_code(&r), "invalid", "op {op} was not refused");
        assert!(
            err_msg(&r).contains(op),
            "refusal for {op} does not name it: {}",
            err_msg(&r)
        );
    }
}

#[test]
fn every_op_refuses_a_field_its_cli_verb_cannot_express() {
    let (_d, f, token) = fixture();
    let ns = open_session(&f, &token);

    for (op, _) in DAEMON_OPS {
        let mut body = minimal_body(op, &ns);
        body.as_object_mut()
            .unwrap()
            .insert("mnt".into(), json!("/somewhere"));
        let r = call(&f, &token, op, body);
        assert_eq!(
            err_code(&r),
            "invalid",
            "op {op} accepted an unknown field instead of refusing it"
        );
        assert!(
            err_msg(&r).contains("mnt"),
            "refusal for {op} does not name the offending field: {}",
            err_msg(&r)
        );
    }
}

/// The concrete shape of the old permissiveness, kept as its own row because
/// it is the one that was actively dangerous: a misspelt `mount` was accepted
/// as a checkin of the DEFAULT mount and answered `ok`, and `attest` was
/// accepted, ignored, and answered `ok` without attesting anything.
#[test]
fn a_misspelt_field_is_never_silently_a_default() {
    let (_d, f, token) = fixture();
    let ns = open_session(&f, &token);
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": ns, "path": "/a.txt", "text": "a"}),
    );

    let typo = call(
        &f,
        &token,
        "ns.checkin",
        json!({"ns": ns, "mnt": "/elsewhere", "msg": "typo"}),
    );
    assert_eq!(err_code(&typo), "invalid");

    // ...and the work is still staged, because nothing was published behind
    // the caller's back.
    let published = ok(
        &f,
        &token,
        "ns.checkin",
        json!({"ns": ns, "mount": "/", "msg": "real"}),
    );
    assert_eq!(published["result"], "updated");
}

#[test]
fn flags_are_booleans_and_arguments_are_strings() {
    let (_d, f, token) = fixture();
    let ns = open_session(&f, &token);

    // A CLI flag is present or absent. "true" is neither.
    let r = call(
        &f,
        &token,
        "ns.mount",
        json!({"ns": ns, "path": "/m", "spec": "ref:main", "rw": "true"}),
    );
    assert_eq!(err_code(&r), "invalid");
    assert!(err_msg(&r).contains("rw"), "{}", err_msg(&r));

    let r = call(
        &f,
        &token,
        "seal",
        json!({"ref": "main", "tag": "flagged", "attest": 1}),
    );
    assert_eq!(err_code(&r), "invalid");

    let r = call(&f, &token, "ns.checkin", json!({"ns": ns, "mount": 3}));
    assert_eq!(err_code(&r), "invalid");
    assert!(err_msg(&r).contains("mount"), "{}", err_msg(&r));

    // And nothing above published a tag on the way to being refused.
    let refs = ok(&f, &token, "refs", json!({}));
    assert!(
        !refs
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "tags/flagged"),
        "a refused seal published a tag: {refs}"
    );
}

#[test]
fn session_open_defaults_from_to_main_like_the_cli() {
    let (_d, f, token) = fixture();
    let implied = ok(&f, &token, "session.open", json!({}));
    let explicit = ok(&f, &token, "session.open", json!({"from": "main"}));
    assert!(implied["ns"].is_string() && explicit["ns"].is_string());
    // Distinct namespaces, both openable: the default resolved, it did not
    // become a missing-argument error.
    assert_ne!(implied["ns"], explicit["ns"]);
}

#[test]
fn checkin_answers_in_the_cli_outcome_vocabulary() {
    let (d, f, token) = fixture();
    let cap = f.root_cap().unwrap();
    f.branch(&cap, "main", "shared").unwrap();

    let seed = ok(&f, &token, "session.open", json!({"from": "shared"}))["ns"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &f,
        &token,
        "ns.mount",
        json!({"ns": seed, "path": "/", "spec": "ref:shared", "rw": true}),
    );
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": seed, "path": "/seed.txt", "text": "v0"}),
    );
    let updated = ok(&f, &token, "ns.checkin", json!({"ns": seed, "msg": "seed"}));
    assert_eq!(updated["result"], "updated");
    assert_eq!(updated["name"], "shared");
    assert_eq!(
        updated["oid"].as_str().map(str::len),
        Some(64),
        "oid must be hex, not a Debug rendering: {updated}"
    );

    // A second checkin of a drained session is the CLI's `noop`.
    let noop = ok(
        &f,
        &token,
        "ns.checkin",
        json!({"ns": seed, "msg": "again"}),
    );
    assert_eq!(noop["result"], "noop");
    assert_eq!(noop["name"], "shared");

    // Two sessions racing one ref: the loser forks (I5/I18), and the daemon
    // reports the fork with the same four names the CLI prints.
    let win = ok(&f, &token, "session.open", json!({"from": "shared"}))["ns"]
        .as_str()
        .unwrap()
        .to_string();
    let lose = ok(&f, &token, "session.open", json!({"from": "shared"}))["ns"]
        .as_str()
        .unwrap()
        .to_string();
    for ns in [&win, &lose] {
        ok(
            &f,
            &token,
            "ns.mount",
            json!({"ns": ns, "path": "/", "spec": "ref:shared", "rw": true}),
        );
    }
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": win, "path": "/w.txt", "text": "w"}),
    );
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": lose, "path": "/l.txt", "text": "l"}),
    );
    ok(&f, &token, "ns.checkin", json!({"ns": win, "msg": "w"}));
    let forked = ok(&f, &token, "ns.checkin", json!({"ns": lose, "msg": "l"}));
    assert_eq!(forked["result"], "forked", "{forked}");
    assert_eq!(forked["requested"], "shared");
    // A SESSION checkin forks inside the losing agent's own scope --
    // `heads/agents/<agent>/forks/<ref>/<ulid>` (I5/I18, #343) -- because I18
    // retargets that session's mount at the fork. The daemon reports the name
    // the CLI prints, so it reports that shape too.
    let fork = forked["fork"].as_str().unwrap_or_default();
    assert!(
        fork.starts_with("heads/agents/") && fork.contains("/forks/shared/"),
        "{forked}"
    );
    assert_eq!(forked["ours"].as_str().map(str::len), Some(64));
    assert_eq!(forked["theirs"].as_str().map(str::len), Some(64));
    drop(d);
}

#[test]
fn checkin_mount_is_the_cli_mount_and_defaults_to_root() {
    let (_d, f, token) = fixture();
    let cap = f.root_cap().unwrap();
    f.branch(&cap, "main", "side").unwrap();

    let ns = open_session(&f, &token);
    ok(
        &f,
        &token,
        "ns.mount",
        json!({"ns": ns, "path": "/side", "spec": "ref:side", "rw": true}),
    );
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": ns, "path": "/side/only.txt", "text": "side"}),
    );

    // The named mount publishes the ref THAT MOUNT names (I19), exactly as
    // `forge checkin --mount /side` does.
    let published = ok(
        &f,
        &token,
        "ns.checkin",
        json!({"ns": ns, "mount": "/side", "msg": "side"}),
    );
    assert_eq!(published["result"], "updated");
    assert_eq!(published["name"], "side");

    // And the default is "/", not "whatever mount happens to hold work".
    let ns2 = open_session(&f, &token);
    ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": ns2, "path": "/root.txt", "text": "root"}),
    );
    let default_mount = ok(&f, &token, "ns.checkin", json!({"ns": ns2, "msg": "root"}));
    assert_eq!(default_mount["result"], "updated");
    assert_eq!(
        default_mount["name"],
        format!("heads/agents/anon/{ns2}"),
        "the default mount is not the CLI's default mount"
    );
}

#[test]
fn seal_attest_actually_attests() {
    let (_d, f, token) = fixture();
    let plain = ok(&f, &token, "seal", json!({"ref": "main", "tag": "plain"}));
    assert_eq!(plain["attested"], false);
    assert_eq!(plain["oid"].as_str().map(str::len), Some(64));

    let attested = ok(
        &f,
        &token,
        "seal",
        json!({"ref": "main", "tag": "attested", "attest": true}),
    );
    assert_eq!(attested["attested"], true);
    // The attestation is the same durable re-read `forge seal --attest` does.
    let cap = f.root_cap().unwrap();
    assert_eq!(
        f.verify_tag(&cap, "attested").unwrap().hex(),
        attested["oid"].as_str().unwrap()
    );

    // Re-sealing a frozen tag is sealed state, exit class 2, not 200.
    let again = call(&f, &token, "seal", json!({"ref": "main", "tag": "plain"}));
    assert_eq!(err_code(&again), "sealed");
    assert_eq!(Error::Sealed(String::new()).exit_code(), 2);

    // And the attestation is really PERFORMED, not merely reported. A
    // capability that may seal but may not read cannot attest: `verify` needs
    // Op::Read on the tag it re-reads (I14/I15). So a request that asks for
    // attestation it cannot have must be refused, exactly as
    // `forge seal --attest` is refused under the same capability -- an
    // `attest` that were still accepted-and-ignored would answer ok here.
    let sealer_only = f
        .grant(
            &f.root_cap().unwrap(),
            vec![
                "ops=seal".to_string(),
                "ref=main,tags/*".to_string(),
                "agent=sealer".to_string(),
            ],
        )
        .unwrap()
        .to_token();
    let unattestable = call(
        &f,
        &sealer_only,
        "seal",
        json!({"ref": "main", "tag": "unattestable", "attest": true}),
    );
    assert_eq!(
        err_code(&unattestable),
        "denied",
        "attest was accepted and not performed: {:?}",
        unattestable.body
    );
    // Without --attest the same capability seals fine, which is what proves
    // the refusal above came from the attestation and not from the seal.
    let plain_enough = ok(
        &f,
        &sealer_only,
        "seal",
        json!({"ref": "main", "tag": "unattested"}),
    );
    assert_eq!(plain_enough["attested"], false);
}

#[test]
fn refs_carries_the_flags_the_cli_prints() {
    let (_d, f, token) = fixture();
    ok(&f, &token, "seal", json!({"ref": "main", "tag": "r1"}));
    let refs = ok(&f, &token, "refs", json!({}));
    let rows = refs.as_array().expect("refs is an array");
    let tag = rows
        .iter()
        .find(|r| r["name"] == "tags/r1")
        .unwrap_or_else(|| panic!("no tags/r1 in {refs}"));
    assert_eq!(tag["kind"], "snapshot");
    assert_eq!(tag["protected"], true);
    assert_eq!(tag["sealed"], true);
    let main = rows.iter().find(|r| r["name"] == "main").unwrap();
    assert_eq!(main["sealed"], false);
}

#[test]
fn write_accepts_the_same_bytes_through_text_and_hex() {
    let (_d, f, token) = fixture();
    let ns = open_session(&f, &token);
    // `hex` is the wire form of `forge write --file`: arbitrary bytes,
    // including bytes no `--text` argument could carry.
    let raw = ok(
        &f,
        &token,
        "ns.write",
        json!({"ns": ns, "path": "/raw.bin", "hex": "00ff10"}),
    );
    assert_eq!(raw["oid"].as_str().map(str::len), Some(64));
    assert_eq!(
        ok(&f, &token, "ns.read", json!({"ns": ns, "path": "/raw.bin"}))["hex"],
        "00ff10"
    );

    let neither = call(&f, &token, "ns.write", json!({"ns": ns, "path": "/x"}));
    assert_eq!(err_code(&neither), "invalid");
}

/// The daemon's `err.code` and its HTTP status are not a second error ABI:
/// each code carries exactly the CLI exit class of the same failure.
///
/// The match below is exhaustive on purpose. Adding an `Error` variant stops
/// this file compiling, which is the only way an error class can never again
/// reach a daemon client without anyone deciding what it means.
#[test]
fn error_codes_carry_the_cli_exit_classification_and_a_status() {
    fn contract(e: &Error) -> (&'static str, u8, u16) {
        match e {
            Error::Denied(_) => ("denied", 1, 403),
            Error::Cap(_) => ("denied", 1, 403),
            Error::NotFound(_) => ("not_found", 1, 404),
            Error::Invalid(_) => ("invalid", 1, 400),
            Error::InvalidBase => ("invalid_base", 1, 409),
            Error::Sealed(_) => ("sealed", 2, 409),
            Error::Corrupt(_) => ("corrupt", 2, 500),
            Error::Busy(_) => ("busy", 3, 503),
            Error::StaleObservation { .. } => ("stale_observation", 4, 409),
            Error::MergeConflict(_) => ("conflict", 4, 409),
            Error::Io(_) => ("internal", 5, 500),
            Error::Sqlite(_) => ("internal", 5, 500),
            Error::Internal(_) => ("internal", 5, 500),
        }
    }

    let every = vec![
        Error::Denied("x".into()),
        Error::Cap("x".into()),
        Error::NotFound("x".into()),
        Error::Invalid("x".into()),
        Error::InvalidBase,
        Error::Sealed("x".into()),
        Error::Corrupt("x".into()),
        Error::Busy("x".into()),
        Error::StaleObservation {
            path: "p".into(),
            expected: "a".into(),
            found: "b".into(),
        },
        Error::MergeConflict(ObjectId::ZERO),
        Error::Io("x".into()),
        Error::Sqlite("x".into()),
        Error::Internal("x".into()),
    ];

    for e in &every {
        let (code, exit, status) = contract(e);
        assert_eq!(e.code(), code, "{e}");
        assert_eq!(e.exit_code(), exit, "{e}");
        let resp = Response::err(1, e);
        assert_eq!(daemon_http_status(&resp), status, "{e}");
        assert_eq!(resp.err.as_ref().unwrap().code, code);
    }
    assert_eq!(daemon_http_status(&Response::ok(1, json!({}))), 200);

    // Exit 5 must be unreachable from caller-controlled input, so no status a
    // daemon client can provoke by shaping a request maps onto it.
    let (_d, f, token) = fixture();
    for (op, body) in [
        ("nope", json!({})),
        (
            "ns.read",
            json!({"ns": "01ZZZZZZZZZZZZZZZZZZZZZZZZ", "path": "/x"}),
        ),
        (
            "ns.checkin",
            json!({"ns": "01ZZZZZZZZZZZZZZZZZZZZZZZZ", "bogus": 1}),
        ),
        ("seal", json!({"ref": "no/such/ref", "tag": "t"})),
        ("session.open", json!({"from": "no-such-ref"})),
    ] {
        let r = call(&f, &token, op, body);
        let code = err_code(&r);
        assert_ne!(code, "internal", "op {op} reached exit 5: {}", err_msg(&r));
        assert_ne!(daemon_http_status(&r), 500, "op {op}: {}", err_msg(&r));
    }
}

#[test]
fn a_bad_capability_is_refused_before_any_op_runs() {
    let (_d, f, token) = fixture();
    // I14: no ambient root. A daemon client without a usable capability gets
    // the same class the CLI gives -- 1 -- whether the token is unparseable
    // (`invalid`) or parses and does not authorise (`denied`).
    let r = call(&f, "not-a-token", "refs", json!({}));
    let code = err_code(&r);
    assert!(
        matches!(code.as_str(), "denied" | "invalid"),
        "unexpected code for a bad capability: {code}"
    );
    assert_eq!(Error::Cap(String::new()).exit_code(), 1);
    assert_eq!(Error::Denied(String::new()).exit_code(), 1);
    assert_eq!(Error::Invalid(String::new()).exit_code(), 1);

    // The capability is loaded before the op is even looked at, so an
    // unauthenticated peer cannot probe which ops exist.
    let unknown_op_no_cap = call(&f, "not-a-token", "definitely.not.an.op", json!({}));
    assert!(!unknown_op_no_cap.ok);
    assert!(
        !err_msg(&unknown_op_no_cap).contains("definitely.not.an.op"),
        "an unauthenticated peer learned whether an op exists: {}",
        err_msg(&unknown_op_no_cap)
    );

    // A well-formed capability that does not authorise the op is `denied`.
    let scoped = f
        .grant(
            &f.root_cap().unwrap(),
            vec!["ops=read".to_string(), "ref=main".to_string()],
        )
        .unwrap()
        .to_token();
    assert_ne!(scoped, token);
    let denied = call(
        &f,
        &scoped,
        "seal",
        json!({"ref": "main", "tag": "unauthorised"}),
    );
    assert_eq!(err_code(&denied), "denied");
}

#[test]
fn a_body_that_is_not_an_object_is_an_input_error() {
    let (_d, f, token) = fixture();
    let r = call(&f, &token, "refs", json!("refs"));
    assert_eq!(err_code(&r), "invalid");
    // An absent body is the empty body, which `refs` accepts.
    assert!(call(&f, &token, "refs", Value::Null).ok);
}

#[test]
fn an_unsupported_protocol_version_is_refused() {
    let (_d, f, token) = fixture();
    let r = dispatch_request(
        &f,
        Request {
            v: 2,
            id: 1,
            op: "refs".into(),
            cap: token,
            body: json!({}),
        },
    );
    assert_eq!(err_code(&r), "invalid");
}
