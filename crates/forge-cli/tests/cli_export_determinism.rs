//! Process-level proof that immutable snapshot export is deterministic under concurrency.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

const EXPORTERS: usize = 8;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn authenticated(dir: &Path, cap: &Path) -> Command {
    let mut cmd = forge();
    cmd.arg("--dir").arg(dir).arg("--cap").arg(cap);
    cmd
}

fn output(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn forge")
}

fn run(cmd: &mut Command) -> String {
    let out = output(cmd);
    assert!(
        out.status.success(),
        "forge failed status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("forge stdout is UTF-8")
}

fn init(dir: &Path) -> (PathBuf, PathBuf) {
    let mut cmd = forge();
    cmd.arg("init").current_dir(dir);
    run(&mut cmd);
    (
        dir.join(".forge/keys/root.cap"),
        dir.join(".forge/keys/integrator.cap"),
    )
}

fn export(dir: &Path, root: &Path, out: &Path) -> Output {
    authenticated(dir, root)
        .arg("export")
        .arg("tags/deterministic")
        .arg("-o")
        .arg(out)
        .output()
        .expect("spawn forge export")
}

#[test]
fn cli_concurrent_exports_of_one_seal_are_byte_identical() {
    let d = tempdir().unwrap();
    let (root, integrator) = init(d.path());

    let source = d.path().join("source");
    fs::create_dir_all(source.join("nested/deeper")).unwrap();
    fs::write(source.join("alpha.txt"), b"alpha\n").unwrap();
    fs::write(source.join("space name.txt"), b"spaces\n").unwrap();
    fs::write(source.join("nested/beta.txt"), b"beta\n").unwrap();
    fs::write(
        source.join("nested/deeper/binary.dat"),
        [0u8, 1, 2, 0xff, 0, 42, 10],
    )
    .unwrap();

    let mut import = authenticated(d.path(), &root);
    import
        .arg("import")
        .arg(&source)
        .arg("--ref")
        .arg("heads/source");
    run(&mut import);

    // Root can read the arbitrary source ref and advance protected main. The
    // built-in integrator is intentionally narrower and is used for sealing.
    let mut merge = authenticated(d.path(), &root);
    merge
        .arg("merge")
        .arg("--into=main")
        .arg("--from=heads/source");
    run(&mut merge);

    let mut seal = authenticated(d.path(), &integrator);
    seal.arg("seal")
        .arg("main")
        .arg("--tag")
        .arg("deterministic")
        .arg("--attest");
    run(&mut seal);

    let barrier = Arc::new(Barrier::new(EXPORTERS));
    let mut launchers = Vec::with_capacity(EXPORTERS);
    let mut paths = Vec::with_capacity(EXPORTERS);
    for i in 0..EXPORTERS {
        let out = d.path().join(format!("parallel-{i}.tar"));
        paths.push(out.clone());
        let barrier = Arc::clone(&barrier);
        let dir = d.path().to_path_buf();
        let cap = root.clone();
        launchers.push(std::thread::spawn(move || {
            barrier.wait();
            export(&dir, &cap, &out)
        }));
    }

    for result in launchers
        .into_iter()
        .map(|launcher| launcher.join().expect("join export launcher"))
    {
        assert!(
            result.status.success(),
            "parallel export failed status={:?}\nstdout={}\nstderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let expected = fs::read(&paths[0]).unwrap();
    assert!(expected.len() > 64, "export unexpectedly empty");
    for path in &paths[1..] {
        assert_eq!(
            fs::read(path).unwrap(),
            expected,
            "concurrent exports of one immutable snapshot differed: {}",
            path.display()
        );
    }

    // Determinism is not scheduler-specific: a later sequential export must
    // produce exactly the same tar bytes too.
    let sequential = d.path().join("sequential.tar");
    let result = export(d.path(), &root, &sequential);
    assert!(result.status.success());
    assert_eq!(fs::read(sequential).unwrap(), expected);

    let mut verify = authenticated(d.path(), &root);
    verify.arg("verify").arg("deterministic");
    run(&mut verify);

    let mut fsck = authenticated(d.path(), &root);
    fsck.arg("fsck").arg("--full");
    run(&mut fsck);
}
