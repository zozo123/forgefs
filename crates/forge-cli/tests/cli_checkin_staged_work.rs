//! #326 / I19 at the CLI boundary: `forge checkin` publishes the mount it is
//! given and refuses, with exit 1 per CLI_ABI.md, while the session holds
//! staged work under any other mount. The refusal is the same answer
//! `forge abandon session` already gives, so the two verbs cannot strand an
//! agent between "nothing to do" and "you still have work".

use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn run(dir: &Path, args: &[&str]) -> Output {
    let mut c = forge();
    c.arg("--dir")
        .arg(dir)
        .arg("--cap")
        .arg(dir.join(".forge/keys/root.cap"));
    c.args(args);
    c.output().expect("spawn forge")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "forge {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn checkin_refuses_exit_one_while_another_mount_holds_staged_work() {
    let d = tempdir().unwrap();
    let repo = d.path().join("r");
    assert!(forge()
        .arg("init")
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    ok(&repo, &["branch", "main", "shared"]);
    let ns = ok(&repo, &["session", "open", "--from", "main"]);
    ok(&repo, &["mount", "--ns", &ns, "/s", "ref:shared", "--rw"]);
    ok(&repo, &["write", "--ns", &ns, "/s/new.txt", "--text", "hi"]);

    let refused = run(&repo, &["checkin", "--ns", &ns, "-m", "a"]);
    let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
    let stdout = String::from_utf8_lossy(&refused.stdout).to_string();
    assert_eq!(
        refused.status.code(),
        Some(1),
        "a checkin that cannot publish the session's staged work is an input error \
         (CLI_ABI.md exit 1), not success: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("noop"),
        "checkin must not print a no-op outcome over staged work: {stdout}"
    );
    assert!(
        stderr.contains("/s (1 staged entry)"),
        "the diagnostic must name the mount holding the work: {stderr}"
    );

    // Abandon agrees with that refusal: the work is still there.
    let abandon = run(&repo, &["abandon", "session", &ns]);
    assert_eq!(abandon.status.code(), Some(1));

    // Naming the mount publishes it, and then both verbs succeed.
    let published = ok(&repo, &["checkin", "--ns", &ns, "--mount", "/s", "-m", "a"]);
    assert!(
        published.starts_with("updated shared ") || published.starts_with("forked "),
        "checkin --mount /s must publish the staged entry: {published}"
    );
    assert_eq!(ok(&repo, &["read", "--ns", &ns, "/s/new.txt"]), "hi");
    ok(&repo, &["abandon", "session", &ns]);
}
