// Ref filtering preserves confidentiality, but the fact that filtering happened is public.
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> Output {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn refs_warns_when_authority_suppresses_rows() {
    let d = tempdir().unwrap();
    run(forge().arg("init").current_dir(d.path()));
    let root = d.path().join(".forge/keys/root.cap");
    let integrator = d.path().join(".forge/keys/integrator.cap");

    run(forge()
        .arg("--dir")
        .arg(d.path())
        .arg("--cap")
        .arg(&root)
        .arg("branch")
        .arg("main")
        .arg("root-only"));

    let listed = run(forge()
        .arg("--dir")
        .arg(d.path())
        .arg("--cap")
        .arg(&integrator)
        .arg("refs"));
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let stderr = String::from_utf8_lossy(&listed.stderr);
    assert!(stdout.contains("main"), "{stdout}");
    assert!(!stdout.contains("root-only"), "{stdout}");
    assert!(
        stderr.contains("1 ref(s) suppressed by authority"),
        "suppression was silent: {stderr}"
    );

    assert!(Path::new(&root).is_file());
}
