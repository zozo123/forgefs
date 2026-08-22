from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one match, found {n}")
    return text.replace(old, new, 1)


def replace_between(text: str, start: str, end: str, replacement: str, label: str) -> str:
    i = text.find(start)
    if i < 0:
        raise SystemExit(f"{label}: start marker missing")
    j = text.find(end, i)
    if j < 0:
        raise SystemExit(f"{label}: end marker missing")
    return text[:i] + replacement + text[j:]


# Use libc only for the two low-level filesystem guarantees std does not expose:
# O_NOFOLLOW for LOCK and atomic no-replace directory publication.
p = Path("Cargo.toml")
s = p.read_text()
s = replace_once(s, 'lru = "0.18.2"\n', 'libc = "0.2"\nlru = "0.18.2"\n', "workspace libc")
p.write_text(s)

p = Path("crates/forge-api/Cargo.toml")
s = p.read_text()
s = replace_once(s, 'getrandom.workspace = true\n', 'getrandom.workspace = true\nlibc.workspace = true\n', "forge-api libc")
p.write_text(s)


p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    'use forge_types::{CasResult, EntryKind, Error, ObjectId, ObjectType, RefRow, Result};\nuse std::fs::{self, File, OpenOptions};\n',
    'use forge_types::{CasResult, EntryKind, Error, ObjectId, ObjectType, RefRow, Result};\nuse std::ffi::CString;\nuse std::fs::{self, File, OpenOptions};\n#[cfg(unix)]\nuse std::os::unix::ffi::OsStrExt;\n#[cfg(unix)]\nuse std::os::unix::fs::{MetadataExt, OpenOptionsExt};\n',
    "api unix imports",
)

new_open_block = r'''impl Forge {
    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        if root.exists() {
            if root.join("VERSION").exists() {
                validate_repo_version(&root)?;
                return Err(Error::Invalid(format!(
                    "already a forge: {}",
                    root.display()
                )));
            }
            return Err(Error::Invalid(format!(
                "{} already exists without a ForgeFS VERSION; refusing to overwrite",
                root.display()
            )));
        }

        // Build completely under a sibling name. Publication is one atomic,
        // no-replace rename; VERSION remains the validity marker written last.
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let base = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".forge");
        let staging = parent.join(format!(
            "{base}.init-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        fs::create_dir(&staging)?;

        let prepared = (|| -> Result<File> {
            secure_key_dir(&staging.join("keys"))?;
            fs::create_dir_all(staging.join("objects"))?;
            fs::create_dir_all(staging.join("tmp"))?;

            let mut hmac_key = [0u8; 32];
            let mut seal_seed = [0u8; 32];
            getrandom::getrandom(&mut hmac_key).map_err(|e| Error::Internal(e.to_string()))?;
            getrandom::getrandom(&mut seal_seed).map_err(|e| Error::Internal(e.to_string()))?;
            let sk = SigningKey::from_bytes(&seal_seed);
            let pk = sk.verifying_key().to_bytes();

            write_secret(staging.join("keys/root.secret"), &hmac_key)?;
            write_secret(staging.join("keys/seal.ed25519"), &seal_seed)?;
            write_public(staging.join("keys/seal.pub"), &pk)?;

            let store = Store::open(&staging)?;
            store.meta.set_cap_root(&pk)?;

            let root_cap = mint_root(&hmac_key)?;
            let integ = mint_integrator(&hmac_key)?;
            write_secret(
                staging.join("keys/root.cap"),
                root_cap.to_token().as_bytes(),
            )?;
            write_secret(
                staging.join("keys/integrator.cap"),
                integ.to_token().as_bytes(),
            )?;
            sync_dir(&staging.join("keys"))?;

            let empty = store.empty_tree_id()?;
            let commit = Commit {
                tree: empty,
                parents: vec![],
                agent: "init".into(),
                msg: "init".into(),
                ts: now_ms(),
                landmark: true,
            };
            let cid = store.put_commit(&commit)?;
            let intro_oids = store.collect_intros(None, empty)?;
            store.meta.insert_ref_with_intros(
                "main",
                cid,
                "commit",
                true,
                false,
                "init",
                "init",
                &intro_oids,
            )?;
            store.meta.landmark(cid, "commit", "init")?;
            drop(store);

            write_public(staging.join("VERSION"), b"1\n")?;
            // Acquire the direct-client shared ownership while LOCK is still in
            // staging. The descriptor keeps the same inode locked across rename,
            // eliminating the post-publication daemon race.
            let cell_lock = acquire_cell_lock(&staging, false)?;
            sync_dir(&staging)?;
            Ok(cell_lock)
        })();

        let cell_lock = match prepared {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };

        if let Err(error) = publish_noreplace(&staging, &root) {
            drop(cell_lock);
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = sync_dir(parent) {
            return Err(Error::Io(format!(
                "repository published at {} but parent directory fsync failed: {error}",
                root.display()
            )));
        }

        // Ownership was acquired before publication; opening the committed cell
        // cannot race a daemon. All repository contents were already validated in
        // staging, so remaining failures are local I/O/corruption, not ambiguity.
        Self::open_locked(root, cell_lock, false)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, false)
    }

    /// Open a cell for `forge serve`. The exclusive lock is acquired before
    /// SQLite or object state is opened, so a daemon can never coexist with a
    /// direct client that holds the shared cell lock.
    pub fn open_for_serve(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, true)
    }

    fn open_with_lock(dir: &Path, exclusive: bool) -> Result<Self> {
        let root = find_forge(dir)?;
        let cell_lock = acquire_cell_lock(&root, exclusive)?;
        Self::open_locked(root, cell_lock, exclusive)
    }

    fn open_locked(root: PathBuf, cell_lock: File, exclusive: bool) -> Result<Self> {
        validate_key_permissions(&root.join("keys"))?;
        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let sk = SigningKey::from_bytes(&seal_seed);
        let seal_pk = sk.verifying_key().to_bytes();
        let store = Store::open(&root)?;
        let configured_pk = store.meta.get_seal_pub()?;
        if configured_pk != seal_pk.to_vec() {
            return Err(Error::Corrupt(
                "configured seal public key does not match local signing key".into(),
            ));
        }
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk,
            root,
            stats: ApiCounters::default(),
            _cell_lock: cell_lock,
            exclusive_cell_lock: exclusive,
        })
    }

'''
s = replace_between(
    s,
    "impl Forge {\n    pub fn init",
    "    pub(crate) fn has_exclusive_cell_lock",
    new_open_block,
    "init/open ownership block",
)

new_lock_helpers = r'''fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid(format!("path contains NUL: {}", path.display())))
}

fn publish_noreplace(from: &Path, to: &Path) -> Result<()> {
    let from_c = path_cstring(from)?;
    let to_c = path_cstring(to)?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };

    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renamex_np(
            from_c.as_ptr(),
            to_c.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    )))]
    return Err(Error::Invalid(
        "atomic no-replace repository publication is unsupported on this platform".into(),
    ));

    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return Err(Error::Invalid(format!("already a forge: {}", to.display())));
    }
    Err(error.into())
}

fn acquire_cell_lock(root: &Path, exclusive: bool) -> Result<File> {
    let path = root.join("LOCK");
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path)?;

    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() || metadata.nlink() != 1 {
            return Err(Error::Denied(format!(
                "LOCK must be a single-link regular file: {}",
                path.display()
            )));
        }
    }

    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Busy(if exclusive {
            "forge cell is in use by a direct client or daemon".into()
        } else {
            "forge daemon owns this cell; use the socket or stop forge serve".into()
        })),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

'''
s = replace_between(
    s,
    "fn acquire_cell_lock(root: &Path, exclusive: bool) -> Result<File> {",
    "fn validate_repo_version(root: &Path) -> Result<()> {",
    new_lock_helpers,
    "lock/publication helpers",
)

# Add deterministic regression tests beside the existing private unit tests.
test_marker = '''    #[test]\n    fn init_and_session_write_checkin() {'''
new_tests = r'''    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn publish_never_replaces_an_existing_empty_root() {
        let d = tempdir().unwrap();
        let staging = d.path().join("staging");
        let root = d.path().join(".forge");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(staging.join("VERSION"), b"1\n").unwrap();

        assert!(publish_noreplace(&staging, &root).is_err());
        assert!(root.is_dir());
        assert!(staging.join("VERSION").exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn staged_shared_lock_remains_authoritative_after_publish() {
        let d = tempdir().unwrap();
        let staging = d.path().join("staging");
        let root = d.path().join(".forge");
        fs::create_dir(&staging).unwrap();
        let shared = acquire_cell_lock(&staging, false).unwrap();
        publish_noreplace(&staging, &root).unwrap();

        assert!(matches!(
            acquire_cell_lock(&root, true),
            Err(Error::Busy(_))
        ));
        drop(shared);
        drop(acquire_cell_lock(&root, true).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let d = tempdir().unwrap();
        let root = d.path().join(".forge");
        fs::create_dir(&root).unwrap();
        let victim = d.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, root.join("LOCK")).unwrap();

        assert!(acquire_cell_lock(&root, true).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }

'''
s = replace_once(s, test_marker, new_tests + test_marker, "hardening unit tests")
p.write_text(s)


# Reject future metadata schema before any state-changing PRAGMA.
p = Path("crates/forge-store/src/meta.rs")
s = p.read_text()
start = "    pub fn open(path: &Path) -> Result<Self> {"
end = "    pub fn set_cap_root(&self, seal_pub: &[u8]) -> Result<()> {"
new_meta_open = r'''    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(map_sql)?;

        // Compatibility checks are read-only. Do not mutate a repository that
        // this binary has already determined it cannot understand.
        let version = schema_version(&conn)?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "metadata schema version {version} is newer than supported {CURRENT_SCHEMA_VERSION}"
            )));
        }

        // Once compatible, establish the durability contract before any schema
        // migration or metadata write.
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(map_sql)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sql)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(map_sql)?;
        #[cfg(target_os = "macos")]
        conn.pragma_update(None, "fullfsync", "ON")
            .map_err(map_sql)?;

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(map_sql)?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Corrupt(format!(
                "metadata durability requires journal_mode=WAL, got {journal_mode}"
            )));
        }
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(map_sql)?;
        if synchronous != 2 {
            return Err(Error::Corrupt(format!(
                "metadata durability requires synchronous=FULL(2), got {synchronous}"
            )));
        }
        #[cfg(target_os = "macos")]
        {
            let fullfsync: i64 = conn
                .pragma_query_value(None, "fullfsync", |row| row.get(0))
                .map_err(map_sql)?;
            if fullfsync != 1 {
                return Err(Error::Corrupt(format!(
                    "metadata durability requires fullfsync=ON on macOS, got {fullfsync}"
                )));
            }
        }

        migrate(&mut conn, version)?;
        conn.execute(
            "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
            [],
        )
        .map_err(map_sql)?;
        Ok(Self {
            write: Mutex::new(conn),
            stats: MetaCounters::default(),
        })
    }

'''
s = replace_between(s, start, end, new_meta_open, "metadata open ordering")
p.write_text(s)


p = Path("crates/forge-store/tests/schema_migrations.rs")
s = p.read_text()
append = r'''

#[test]
fn newer_schema_rejection_does_not_mutate_journal_mode() {
    let d = tempdir().unwrap();
    let path = d.path().join("future.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);\
         INSERT INTO schema_migrations (version, applied_ms) VALUES (2, 0);",
    )
    .unwrap();
    let before: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(before.to_ascii_lowercase(), "delete");
    drop(conn);

    let err = Meta::open(&path)
        .err()
        .expect("future schema must fail before durability mutation");
    assert!(matches!(err, Error::Invalid(_)), "unexpected error: {err}");

    let conn = Connection::open(&path).unwrap();
    let after: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(after.to_ascii_lowercase(), "delete");
    assert!(!d.path().join("future.sqlite-wal").exists());
}
'''
if "newer_schema_rejection_does_not_mutate_journal_mode" in s:
    raise SystemExit("schema regression test already exists")
p.write_text(s.rstrip() + append + "\n")
