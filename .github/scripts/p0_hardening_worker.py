from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def replace_between(text: str, start: str, end: str, new: str, label: str) -> str:
    i = text.find(start)
    if i < 0:
        raise SystemExit(f"{label}: start marker not found")
    j = text.find(end, i)
    if j < 0:
        raise SystemExit(f"{label}: end marker not found")
    return text[:i] + new + text[j:]


# forge-api: atomic init publication, strict VERSION, shared/exclusive cell locks,
# lossless directory enumeration, and no dead generated config.
path = Path("crates/forge-api/src/lib.rs")
s = path.read_text()
s = replace_once(s, "use std::fs;\n", "use std::fs::{self, File, OpenOptions};\n", "api fs import")
s = replace_once(s, "use std::io::Write;\n", "use std::io::Write;\n", "api io import")
s = replace_once(
    s,
    "    stats: ApiCounters,\n}\n",
    "    stats: ApiCounters,\n    // Shared for direct clients, exclusive for the daemon. The descriptor lifetime is the lock.\n    _cell_lock: File,\n    exclusive_cell_lock: bool,\n}\n",
    "Forge lock fields",
)

new_impl_prefix = r'''impl Forge {
    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        if root.exists() {
            if root.join("VERSION").exists() {
                validate_repo_version(&root)?;
                return Err(Error::Invalid(format!("already a forge: {}", root.display())));
            }
            return Err(Error::Invalid(format!(
                "{} already exists without a ForgeFS VERSION; refusing to overwrite",
                root.display()
            )));
        }

        // Build a complete repository under a sibling staging name. VERSION is
        // written last, then the directory rename publishes the repository in
        // one namespace operation. A crash before rename leaves no `.forge`
        // validity marker, so a retry is safe.
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

        let initialized = (|| -> Result<()> {
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

            // VERSION is the validity/compatibility marker and is published last.
            write_public(staging.join("VERSION"), b"1\n")?;
            sync_dir(&staging)?;
            Ok(())
        })();

        if let Err(error) = initialized {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        if let Err(error) = fs::rename(&staging, &root) {
            let _ = fs::remove_dir_all(&staging);
            if root.exists() {
                return Err(Error::Invalid(format!("already a forge: {}", root.display())));
            }
            return Err(error.into());
        }
        sync_dir(parent)?;
        Self::open(&root)
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

    pub(crate) fn has_exclusive_cell_lock(&self) -> bool {
        self.exclusive_cell_lock
    }

'''
s = replace_between(
    s,
    "impl Forge {\n    pub fn init",
    "    pub fn root(&self) -> &Path {",
    new_impl_prefix,
    "Forge init/open block",
)
s = replace_once(
    s,
    "        let tree = import_walk(&self.store, dir)?;\n",
    "        let tree = import_walk(&self.store, dir, true)?;\n",
    "import root call",
)
old_import = r'''fn import_walk(store: &Store, dir: &Path) -> Result<ObjectId> {
    let mut entries = Vec::new();
    let mut kids: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    kids.sort_by_key(|e| e.file_name());
    for k in kids {
        let name = k
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        if name == ".forge" || name == ".git" {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_symlink() {
            return Err(Error::Invalid(format!(
                "import refuses symlink {}",
                k.path().display()
            )));
        }
        if ft.is_dir() {
            let id = import_walk(store, &k.path())?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "import refuses unsupported file type {}",
                k.path().display()
            )));
        } else {
            let data = fs::read(k.path())?;
            let id = store.put_blob_data(&data)?;
            let exec = is_exec(&k.path());
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Blob,
                id,
                exec,
            });
        }
    }
    store.put_tree(&Tree::new(entries)?)
}

fn is_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        false
    }
}
'''
new_import = r'''fn import_walk(store: &Store, dir: &Path, source_root: bool) -> Result<ObjectId> {
    let mut entries = Vec::new();
    // Never turn a per-entry enumeration error into a successful partial import.
    let mut kids: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    kids.sort_by_key(|e| e.file_name());
    for k in kids {
        let name = k
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid(format!("non-utf8 name in {}", dir.display())))?;
        // Root control directories are outside the import domain. Nested names
        // with the same spelling are ordinary user data and must be preserved.
        if source_root && (name == ".forge" || name == ".git") {
            continue;
        }
        let ft = k.file_type()?;
        if ft.is_symlink() {
            return Err(Error::Invalid(format!(
                "import refuses symlink {}",
                k.path().display()
            )));
        }
        if ft.is_dir() {
            let id = import_walk(store, &k.path(), false)?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Tree,
                id,
                exec: false,
            });
        } else if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "import refuses unsupported file type {}",
                k.path().display()
            )));
        } else {
            let data = fs::read(k.path())?;
            let id = store.put_blob_data(&data)?;
            let exec = is_exec(&k.path())?;
            entries.push(forge_core::TreeEntry {
                name,
                kind: forge_types::EntryKind::Blob,
                id,
                exec,
            });
        }
    }
    store.put_tree(&Tree::new(entries)?)
}

fn is_exec(p: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(p)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(false)
    }
}
'''
s = replace_once(s, old_import, new_import, "lossless import block")

helpers = r'''fn acquire_cell_lock(root: &Path, exclusive: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("LOCK"))?;
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(Error::Busy(if exclusive {
                "forge cell is in use by a direct client or daemon".into()
            } else {
                "forge daemon owns this cell; use the socket or stop forge serve".into()
            }));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }

    if exclusive {
        file.set_len(0)?;
        let mut handle = &file;
        handle.write_all(std::process::id().to_string().as_bytes())?;
        file.sync_all()?;
    }
    Ok(file)
}

fn validate_repo_version(root: &Path) -> Result<()> {
    let version = fs::read(root.join("VERSION"))?;
    match version.as_slice() {
        b"1" | b"1\n" | b"1\r\n" => Ok(()),
        _ => Err(Error::Invalid(format!(
            "unsupported ForgeFS repository VERSION at {} (this binary supports VERSION 1)",
            root.display()
        ))),
    }
}

'''
s = replace_once(s, "fn forge_root(dir: &Path) -> PathBuf {\n", helpers + "fn forge_root(dir: &Path) -> PathBuf {\n", "cell/version helpers")
s = replace_once(
    s,
    "        if cand.join(\"VERSION\").exists() {\n            return Ok(cand);\n        }\n",
    "        if cand.join(\"VERSION\").exists() {\n            validate_repo_version(&cand)?;\n            return Ok(cand);\n        }\n",
    "find_forge version validation",
)
path.write_text(s)


# forge-api serve: lock authority is held by Forge::open_for_serve for the
# complete process lifetime. Never unlink the LOCK rendezvous pathname.
path = Path("crates/forge-api/src/serve.rs")
s = path.read_text()
s = replace_once(s, "use std::fs::OpenOptions;\n", "", "serve OpenOptions import")
old = r'''pub fn serve(forge: Arc<Forge>, http: bool) -> Result<()> {
    let root = forge.root().to_path_buf();
    let lock = root.join("LOCK");

    // The file is only a rendezvous point; the OS-held lock is the authority.
    // A crashed daemon may leave LOCK behind, but the kernel releases the lock
    // with the file descriptor, so the next daemon can recover safely.
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?;
    lock_file
        .try_lock()
        .map_err(|_| Error::Busy("forge daemon lock is already held".into()))?;
    lock_file.set_len(0)?;
    let pid = std::process::id().to_string();
    std::io::Write::write_all(&mut &lock_file, pid.as_bytes())?;
    lock_file.sync_all()?;

    let sock_path = root.join("forge.sock");
    // We hold the exclusive daemon lock, so any remaining socket pathname is
    // stale. Removing it here cannot race another valid Forge daemon.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;
'''
new = r'''pub fn serve(forge: Arc<Forge>, http: bool) -> Result<()> {
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
'''
s = replace_once(s, old, new, "serve lock block")
s = replace_once(
    s,
    "    // Keep lock_file alive for the complete service lifetime.\n    let result = accept_loop(forge, listener);\n    let _ = lock_file.unlock();\n    let _ = std::fs::remove_file(&sock_path);\n    let _ = std::fs::remove_file(&lock);\n    result\n",
    "    // Forge owns the exclusive LOCK descriptor for the service lifetime.\n    // Keep the LOCK pathname persistent: unlinking it after unlock can split the\n    // rendezvous inode from a concurrently opening client.\n    let result = accept_loop(forge, listener);\n    let _ = std::fs::remove_file(&sock_path);\n    result\n",
    "serve cleanup block",
)
path.write_text(s)


# forge-cli: acquire exclusive ownership before opening state for serve.
path = Path("crates/forge-cli/src/main.rs")
s = path.read_text()
s = replace_once(
    s,
    "            let f = Arc::new(open(&cli)?);\n            eprintln!(\"forge serve {}\", f.root().display());\n            forge_api::serve(f, http)\n",
    "            let dir = cli.dir.clone().unwrap_or_else(|| PathBuf::from(\".\"));\n            let f = Arc::new(Forge::open_for_serve(&dir)?);\n            eprintln!(\"forge serve {}\", f.root().display());\n            forge_api::serve(f, http)\n",
    "cli serve open",
)
path.write_text(s)


# forge-store: set durability before any DDL/migration and keep schema creation
# inside the v0->v1 migration transaction instead of pre-applying latest DDL.
path = Path("crates/forge-store/src/meta.rs")
s = path.read_text()
s = replace_once(
    s,
    "    let tx = conn\n        .transaction_with_behavior(TransactionBehavior::Immediate)\n        .map_err(map_sql)?;\n    tx.execute(\n        \"INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, ?2)\",\n        params![CURRENT_SCHEMA_VERSION, now_ms() as i64],\n    )\n",
    "    let tx = conn\n        .transaction_with_behavior(TransactionBehavior::Immediate)\n        .map_err(map_sql)?;\n    tx.execute_batch(SCHEMA).map_err(map_sql)?;\n    tx.execute(\n        \"INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, ?2)\",\n        params![CURRENT_SCHEMA_VERSION, now_ms() as i64],\n    )\n",
    "schema migration owns DDL",
)
new_open = r'''    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(map_sql)?;

        // Durability is part of the catalog contract, not an inherited SQLite
        // default. Establish it before any schema or metadata write.
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

        let version = schema_version(&conn)?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "metadata schema version {version} is newer than supported {CURRENT_SCHEMA_VERSION}"
            )));
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
s = replace_between(
    s,
    "    pub fn open(path: &Path) -> Result<Self> {\n",
    "    pub fn set_cap_root(&self, seal_pub: &[u8]) -> Result<()> {\n",
    new_open,
    "Meta::open",
)
if "mod durability_policy_tests" in s:
    raise SystemExit("durability tests already present")
s += r'''

#[cfg(test)]
mod durability_policy_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_enforces_catalog_durability_pragmas() {
        let dir = tempdir().unwrap();
        let meta = Meta::open(&dir.path().join("meta.sqlite")).unwrap();
        let conn = meta.write.lock();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 2, "FULL is SQLite value 2");
        #[cfg(target_os = "macos")]
        {
            let fullfsync: i64 = conn
                .pragma_query_value(None, "fullfsync", |row| row.get(0))
                .unwrap();
            assert_eq!(fullfsync, 1);
        }
    }
}
'''
path.write_text(s)


# Public bootstrap/format contract tests.
Path("crates/forge-api/tests/bootstrap_contract.rs").write_text(r'''use forge_api::Forge;
use forge_types::Error;
use std::fs;
use tempfile::tempdir;

#[test]
fn init_publishes_only_a_complete_versioned_repository() {
    let dir = tempdir().unwrap();
    let stale = dir.path().join(".forge.init-dead-worker");
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("junk"), b"partial").unwrap();

    let forge = Forge::init(dir.path()).unwrap();
    assert_eq!(fs::read(forge.root().join("VERSION")).unwrap(), b"1\n");
    assert!(!forge.root().join("config.toml").exists());
    assert!(forge.root().join("keys/root.cap").exists());
    assert!(forge.root().join("meta.sqlite").exists());
    forge.root_cap().unwrap();
    drop(forge);

    // A pre-publication crash leaves only a sibling staging directory; it does
    // not poison discovery or prevent a fully initialized repository opening.
    assert!(stale.join("junk").exists());
    drop(Forge::open(dir.path()).unwrap());
}

#[test]
fn init_never_overwrites_an_unversioned_dot_forge() {
    let dir = tempdir().unwrap();
    let root = dir.path().join(".forge");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sentinel"), b"keep").unwrap();
    let error = Forge::init(dir.path()).err().expect("must fail closed");
    assert!(matches!(error, Error::Invalid(_)), "{error}");
    assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn future_or_malformed_repository_version_fails_closed() {
    for bytes in [b"2\n".as_slice(), b"garbage\n".as_slice(), b"1  ".as_slice()] {
        let dir = tempdir().unwrap();
        drop(Forge::init(dir.path()).unwrap());
        fs::write(dir.path().join(".forge/VERSION"), bytes).unwrap();
        let error = Forge::open(dir.path()).err().expect("unsupported VERSION must fail");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
    }
}

#[test]
fn import_excludes_only_root_control_directories() {
    let source = tempdir().unwrap();
    fs::create_dir_all(source.path().join(".git")).unwrap();
    fs::write(source.path().join(".git/root-control"), b"skip").unwrap();
    fs::create_dir_all(source.path().join("nested/.git")).unwrap();
    fs::write(source.path().join("nested/.git/keep"), b"keep").unwrap();

    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let cap = forge.root_cap().unwrap();
    forge
        .import_dir(&cap, source.path(), "heads/import")
        .unwrap();
    let out = dir.path().join("import.tar");
    forge.export_tar(&cap, "heads/import", &out).unwrap();

    let file = fs::File::open(out).unwrap();
    let mut archive = tar::Archive::new(file);
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string()
        })
        .collect();
    assert!(names.iter().any(|name| name == "nested/.git/keep"), "{names:?}");
    assert!(!names.iter().any(|name| name == ".git/root-control"), "{names:?}");
}
''')


# Process-level cell-ownership proof: daemon excludes direct clients and a
# killed daemon cannot brick restart. Existing e2e still proves direct/direct
# shared-lock concurrency.
Path("crates/forge-cli/tests/cell_lock.rs").write_text(r'''use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

fn run_ok(cmd: &mut Command) -> String {
    let output = cmd.output().expect("spawn forge");
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn spawn_serve(dir: &str) -> Child {
    forge()
        .args(["--dir", dir, "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forge serve")
}

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon socket did not appear: {}", path.display());
}

#[test]
fn daemon_and_direct_clients_cannot_split_brain() {
    let temp = tempdir().unwrap();
    run_ok(forge().arg("init").current_dir(temp.path()));
    let dir = temp.path().to_str().unwrap();
    let cap = temp.path().join(".forge/keys/root.cap");
    let cap = cap.to_str().unwrap();
    let socket = temp.path().join(".forge/forge.sock");

    let mut daemon = spawn_serve(dir);
    wait_for_socket(&socket);

    let direct = forge()
        .args(["--dir", dir, "--cap", cap, "refs"])
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&direct.stderr).contains("daemon"),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(socket.exists(), "direct open must never unlink a live daemon socket");

    let second = forge().args(["--dir", dir, "serve"]).output().unwrap();
    assert_eq!(second.status.code(), Some(3));

    daemon.kill().unwrap();
    daemon.wait().unwrap();

    // Kernel ownership, not the stale socket/LOCK pathname, decides authority.
    run_ok(forge().args(["--dir", dir, "--cap", cap, "refs"]));
    assert!(temp.path().join(".forge/LOCK").exists());

    let mut restarted = spawn_serve(dir);
    wait_for_socket(&socket);
    restarted.kill().unwrap();
    restarted.wait().unwrap();
}
''')
