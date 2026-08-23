use forge_api::Forge;
use std::fs;
use tempfile::tempdir;

fn import_fails(source: &std::path::Path) {
    let dst = tempdir().unwrap();
    let forge = Forge::init(dst.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    assert!(
        forge.import_dir(&cap, source, "heads/import").is_err(),
        "unsupported or unstable source must fail closed"
    );
}

#[cfg(unix)]
#[test]
fn import_rejects_symlink_fifo_socket_and_non_utf8_name() {
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    {
        let source = tempdir().unwrap();
        fs::write(source.path().join("target"), b"data").unwrap();
        symlink("target", source.path().join("link")).unwrap();
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let fifo = source.path().join("pipe");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let _listener = UnixListener::bind(source.path().join("sock")).unwrap();
        import_fails(source.path());
    }
    {
        let source = tempdir().unwrap();
        let bad = OsString::from_vec(b"bad-\xff".to_vec());
        match fs::write(source.path().join(bad), b"data") {
            Ok(()) => import_fails(source.path()),
            // APFS may reject the fixture before ForgeFS can observe it. That
            // is already fail-closed; accept only the platform's invalid-byte
            // errors rather than skipping the whole Unix adapter test.
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code) if code == libc::EILSEQ || code == libc::EINVAL
                ) => {}
            Err(error) => panic!("unexpected non-UTF-8 fixture error: {error}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn import_rejects_a_regular_file_mutating_during_snapshot() {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };
    use std::thread;
    use std::time::Duration;

    let source = tempdir().unwrap();
    let path = source.path().join("moving.bin");
    fs::write(&path, vec![0u8; 32 * 1024 * 1024]).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let writer_path = path.clone();
    let writer_stop = Arc::clone(&stop);
    let writer_barrier = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .unwrap();
        let mut byte = 1u8;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte]).unwrap();
        writer_barrier.wait();
        while !writer_stop.load(Ordering::Relaxed) {
            byte = byte.wrapping_add(1);
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[byte]).unwrap();
            std::hint::spin_loop();
        }
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(10));
    let dst = tempdir().unwrap();
    let forge = Forge::init(dst.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    let result = forge.import_dir(&cap, source.path(), "heads/import");
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(
        result.is_err(),
        "a changing source file must never import successfully"
    );
}
