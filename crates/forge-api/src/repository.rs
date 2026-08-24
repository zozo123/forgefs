//! Repository discovery, atomic initialization, cell ownership, keys, and durability.

use crate::{ApiCounters, Forge};
use ed25519_dalek::SigningKey;
use forge_cap::{mint_integrator, mint_root};
use forge_core::{now_ms, Commit};
use forge_store::Store;
use forge_types::{Error, Result};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

impl Forge {
    pub fn init(dir: &Path) -> Result<Self> {
        let root = forge_root(dir);
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_dir_all_durable(parent)?;

        // Serialize initializers on the repository's parent directory itself:
        // no persistent lock artifact is needed, and a crash releases the
        // kernel lock. Once held, every matching sibling staging directory is
        // from a previous failed initializer and can be reclaimed without
        // racing another current ForgeFS initializer.
        let _init_parent_lock = acquire_init_parent_lock(parent)?;
        cleanup_init_staging_siblings(&root)?;

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
        init_crash_point("staging-created");

        let prepared = (|| -> Result<Option<File>> {
            secure_key_dir(&staging.join("keys"))?;
            fs::create_dir_all(staging.join("objects"))?;
            fs::create_dir_all(staging.join("tmp"))?;
            init_crash_point("directories-created");

            let mut hmac_key = [0u8; 32];
            let mut seal_seed = [0u8; 32];
            getrandom::getrandom(&mut hmac_key).map_err(|e| Error::Internal(e.to_string()))?;
            getrandom::getrandom(&mut seal_seed).map_err(|e| Error::Internal(e.to_string()))?;
            let sk = SigningKey::from_bytes(&seal_seed);
            let pk = sk.verifying_key().to_bytes();

            write_secret(staging.join("keys/root.secret"), &hmac_key)?;
            write_secret(staging.join("keys/seal.ed25519"), &seal_seed)?;
            write_public(staging.join("keys/seal.pub"), &pk)?;
            init_crash_point("keys-written");

            let store = Store::open(&staging)?;
            store.meta.set_cap_root(&pk)?;
            init_crash_point("catalog-created");

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
            sync_dir_at(
                &staging.join("keys"),
                forge_store::DurabilityBarrier::InitKeyDirectory,
            )?;

            let empty = store.empty_tree_id()?;
            let commit = Commit {
                tree: empty,
                parents: vec![],
                agent: "init".into(),
                msg: "init".into(),
                ts: now_ms(),
                landmark: true,
                contrib: None,
            };
            let cid = store.put_commit(&commit)?;
            init_crash_point("initial-objects-written");
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
            init_crash_point("main-ref-written");
            drop(store);

            write_public(staging.join("VERSION"), b"1\n")?;
            init_crash_point("version-written");
            // Acquire the direct-client shared ownership while LOCK is still in
            // staging. The descriptor keeps the same inode locked across rename,
            // eliminating the post-publication daemon race.
            let cell_lock = acquire_cell_lock(&staging, LockIntent::Create)?;
            sync_dir_at(
                &staging,
                forge_store::DurabilityBarrier::InitStagingDirectory,
            )?;
            init_crash_point("staging-durable");
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
        // The rename is now atomically visible to concurrent processes. The
        // parent-directory barrier below is still required for power-loss
        // durability; the failpoint models process death, not power failure.
        init_crash_point("published");
        if let Err(error) = sync_dir_at(
            parent,
            forge_store::DurabilityBarrier::InitPublicationDirectory,
        ) {
            return Err(Error::Io(format!(
                "repository published at {} but parent directory fsync failed: {error}",
                root.display()
            )));
        }
        init_crash_point("parent-durable");

        // Ownership was acquired before publication; opening the committed cell
        // cannot race a daemon. All repository contents were already validated in
        // staging, so remaining failures are local I/O/corruption, not ambiguity.
        Self::open_locked(root, cell_lock, false)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, false)
    }

    /// Open a cell for the documented read-only commands (`fsck`, `verify`)
    /// without writing a single byte: LOCK is opened `O_RDONLY` and never
    /// created, SQLite is opened `SQLITE_OPEN_READONLY` with `query_only=1`,
    /// no durability pragma is set, no migration runs, no `cap_root` scrub
    /// runs, and no object/tmp directory is created or reclaimed. It is the
    /// only open that works when the media itself is mounted read-only.
    ///
    /// The mode is selected explicitly by the caller rather than by sniffing
    /// `EROFS`: auto-degrading would turn a writable-intent open on
    /// accidentally read-only media into a handle that silently cannot commit,
    /// and would make the guarantee depend on the medium instead of on the
    /// command. Every write through this handle is refused with
    /// `Error::Denied` at the store boundary.
    pub fn open_read_only(dir: &Path) -> Result<Self> {
        let root = find_forge(dir)?;
        let cell_lock = acquire_cell_lock(&root, LockIntent::ReadOnly)?;
        Self::open_locked_mode(root, cell_lock, false, true)
    }

    /// Open a cell for `forge serve`. The exclusive lock is acquired before
    /// SQLite or object state is opened, so a daemon can never coexist with a
    /// direct client that holds the shared cell lock.
    pub fn open_for_serve(dir: &Path) -> Result<Self> {
        Self::open_with_lock(dir, true)
    }

    fn open_with_lock(dir: &Path, exclusive: bool) -> Result<Self> {
        let root = find_forge(dir)?;
        let intent = if exclusive {
            LockIntent::Exclusive
        } else {
            LockIntent::Shared
        };
        let cell_lock = acquire_cell_lock(&root, intent)?;
        Self::open_locked(root, cell_lock, exclusive)
    }

    fn open_locked(root: PathBuf, cell_lock: Option<File>, exclusive: bool) -> Result<Self> {
        Self::open_locked_mode(root, cell_lock, exclusive, false)
    }

    fn open_locked_mode(
        root: PathBuf,
        cell_lock: Option<File>,
        exclusive: bool,
        read_only: bool,
    ) -> Result<Self> {
        // Revalidate after acquiring ownership. An updater may replace the
        // repository between discovery's read-only check and this lock grant.
        // No key, object, or SQLite state may be read or mutated first.
        validate_repo_version(&root)?;
        // A previous initializer may have died after the no-replace rename but
        // before forcing this directory entry. Every cold open joins that
        // publication before exposing a handle that can acknowledge writes.
        // A read-only handle acknowledges no write and cannot take a durability
        // barrier on read-only media, so it has nothing to join.
        if !read_only {
            sync_repo_parent(&root)?;
        }
        validate_key_permissions(&root.join("keys"))?;
        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let sk = SigningKey::from_bytes(&seal_seed);
        let seal_pk = sk.verifying_key().to_bytes();
        let store = if read_only {
            Store::open_read_only(&root)?
        } else {
            Store::open(&root)?
        };
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
            read_only,
        })
    }

    pub(crate) fn has_exclusive_cell_lock(&self) -> bool {
        self.exclusive_cell_lock
    }

    /// True when this handle was opened read-only. Writes through it are
    /// refused with `Error::Denied` by the object store and by SQLite's
    /// `query_only`; this is a diagnostic, not the enforcement point.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
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

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(Error::Invalid(format!("already a forge: {}", to.display())));
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if error.raw_os_error() == Some(libc::EINVAL) {
            return Err(Error::Invalid(
                "filesystem does not support atomic no-replace repository publication".into(),
            ));
        }
        Err(error.into())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = (from_c, to_c);
        Err(Error::Invalid(
            "atomic no-replace repository publication is unsupported on this platform".into(),
        ))
    }
}

/// Why a cell lock is being taken. The open mode of `.forge/LOCK` is part of
/// the contract, not an incidental detail: `flock` needs neither write access
/// nor an `O_CREAT`, so only an intent that may itself write the repository
/// opens LOCK for writing or creates it. Anything stricter makes a repository
/// on read-only media impossible to open at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LockIntent {
    /// `Forge::init`: LOCK does not exist yet, so it must be created, then held
    /// shared across publication.
    Create,
    /// A direct client that may write. Shared ownership of an existing LOCK.
    Shared,
    /// `forge serve`: exclusive ownership, excluding direct clients.
    Exclusive,
    /// `fsck`/`verify`: shared ownership, opened read-only, and tolerated when
    /// the media refuses the open outright.
    ReadOnly,
}

impl LockIntent {
    fn exclusive(self) -> bool {
        matches!(self, Self::Exclusive)
    }

    /// Only these may write to or create the LOCK file itself.
    fn may_write_lock_file(self) -> bool {
        matches!(self, Self::Create | Self::Exclusive)
    }
}

/// Read-only media can refuse to hand out a descriptor for LOCK at all. `flock`
/// is advisory and a read-only handle mutates nothing, so a reader continues
/// unlocked rather than failing: refusing here is what made the documented
/// read-only `fsck` and `verify` impossible on read-only media.
fn lock_open_tolerable(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EROFS)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn acquire_cell_lock(root: &Path, intent: LockIntent) -> Result<Option<File>> {
    let path = root.join("LOCK");
    let mut options = OpenOptions::new();
    options.read(true);
    if intent.may_write_lock_file() {
        options.write(true).create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if intent == LockIntent::ReadOnly && lock_open_tolerable(&error) => {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };

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

    let exclusive = intent.exclusive();
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Busy(if exclusive {
            "forge cell is in use by a direct client or daemon".into()
        } else {
            "forge daemon owns this cell; use the socket or stop forge serve".into()
        })),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(crate) fn init_staging_siblings(root: &Path) -> Result<Vec<PathBuf>> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".forge");
    let prefix = format!("{base}.init-");
    let mut paths = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((pid, ulid)) = suffix.split_once('-') else {
            continue;
        };
        if pid.is_empty()
            || !pid.bytes().all(|byte| byte.is_ascii_digit())
            || ulid.parse::<ulid::Ulid>().is_err()
        {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn cleanup_init_staging_siblings(root: &Path) -> Result<()> {
    let paths = init_staging_siblings(root)?;
    if paths.is_empty() {
        return Ok(());
    }
    for path in &paths {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(Error::Invalid(format!(
                "reserved ForgeFS init staging path is not a directory: {}",
                path.display()
            )));
        }
        fs::remove_dir_all(path)?;
    }
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_dir_at(
        parent,
        forge_store::DurabilityBarrier::InitCleanupDirectory,
    )?;
    Ok(())
}

fn acquire_init_parent_lock(parent: &Path) -> Result<File> {
    let file = File::open(parent)?;
    if !file.metadata()?.is_dir() {
        return Err(Error::Invalid(format!(
            "ForgeFS init parent is not a directory: {}",
            parent.display()
        )));
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Busy(
            "another ForgeFS initializer owns this directory".into(),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn validate_repo_version(root: &Path) -> Result<()> {
    let path = root.join("VERSION");
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.len() > 3 {
        return Err(Error::Invalid(format!(
            "ForgeFS VERSION must be a regular file of at most 3 bytes: {}",
            path.display()
        )));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = options.open(&path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || opened_metadata.len() > 3 {
        return Err(Error::Invalid(format!(
            "ForgeFS VERSION changed while opening or is not a bounded regular file: {}",
            path.display()
        )));
    }
    let mut version = [0u8; 4];
    let read = file.read(&mut version)?;
    match &version[..read] {
        b"1" | b"1\n" | b"1\r\n" => Ok(()),
        _ => Err(Error::Invalid(format!(
            "unsupported ForgeFS repository VERSION at {} (this binary supports VERSION 1)",
            root.display()
        ))),
    }
}

fn sync_repo_parent(root: &Path) -> Result<()> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_dir_at(
        parent,
        forge_store::DurabilityBarrier::OpenPublicationDirectory,
    )
}

/// Create only missing parent components and force each new directory entry
/// before building below it. Existing user-supplied ancestors are assumed to
/// predate this operation; ForgeFS owns and re-proves the final `.forge` edge.
fn create_dir_all_durable(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        match fs::metadata(&current) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {
                return Err(Error::Invalid(format!(
                    "init parent component is not a directory: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !fs::metadata(&current)?.is_dir() {
                    return Err(Error::Invalid(format!(
                        "init parent component is not a directory: {}",
                        current.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let parent = current.parent().unwrap_or_else(|| Path::new("."));
        sync_dir_at(
            parent,
            forge_store::DurabilityBarrier::InitParentDirectory,
        )?;
    }
    Ok(())
}

fn forge_root(dir: &Path) -> PathBuf {
    if dir.ends_with(".forge") {
        dir.to_path_buf()
    } else {
        dir.join(".forge")
    }
}

pub fn find_forge(start: &Path) -> Result<PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    loop {
        let cand = if cur.ends_with(".forge") {
            cur.clone()
        } else {
            cur.join(".forge")
        };
        match fs::symlink_metadata(&cand) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() {
                    return Err(Error::Invalid(format!(
                        "{} is not a ForgeFS repository directory",
                        cand.display()
                    )));
                }
                if !cand.join("VERSION").exists() {
                    return Err(Error::Invalid(format!(
                        "{} exists without a ForgeFS VERSION; refusing to search a parent repository",
                        cand.display()
                    )));
                }
                validate_repo_version(&cand)?;
                return Ok(cand);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if !cur.pop() {
            break;
        }
    }
    Err(Error::NotFound(format!(
        "no .forge above {}",
        start.display()
    )))
}

/// Debug-build-only process crash hook used by the cross-process init matrix.
/// `_exit` skips Rust and SQLite destructors, matching abrupt process loss while
/// keeping production builds free of an environment-controlled exit path.
fn init_crash_point(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("FORGEFS_TEST_INIT_CRASH_AFTER")
        .ok()
        .as_deref()
        == Some(point)
    {
        #[cfg(unix)]
        unsafe {
            libc::_exit(86)
        }
        #[cfg(not(unix))]
        std::process::exit(86);
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
}

fn secure_key_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_key_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(Error::Denied(format!(
                "key directory {} must be mode 0700, got {mode:04o}",
                path.display()
            )));
        }
        for name in ["root.secret", "seal.ed25519", "root.cap", "integrator.cap"] {
            let secret = path.join(name);
            let mode = fs::metadata(&secret)?.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(Error::Denied(format!(
                    "secret {} must be mode 0600, got {mode:04o}",
                    secret.display()
                )));
            }
        }
    }
    Ok(())
}

fn write_secret(path: PathBuf, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        opts.open(&path)?
    };
    #[cfg(not(unix))]
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    forge_store::durable_sync_file_at(&f, forge_store::DurabilityBarrier::InitFile)?;
    Ok(())
}

fn write_public(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    forge_store::durable_sync_file_at(&f, forge_store::DurabilityBarrier::InitFile)?;
    Ok(())
}

fn sync_dir_at(path: &Path, point: forge_store::DurabilityBarrier) -> Result<()> {
    forge_store::durable_sync_dir_at(path, point)
}

fn read32(path: &Path) -> Result<[u8; 32]> {
    let v = fs::read(path)?;
    if v.len() != 32 {
        return Err(Error::Corrupt(format!(
            "expected 32 bytes in {}",
            path.display()
        )));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
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
        let shared = acquire_cell_lock(&staging, LockIntent::Create).unwrap();
        publish_noreplace(&staging, &root).unwrap();

        assert!(matches!(
            acquire_cell_lock(&root, LockIntent::Exclusive),
            Err(Error::Busy(_))
        ));
        drop(shared);
        drop(acquire_cell_lock(&root, LockIntent::Exclusive).unwrap());
    }

    #[test]
    fn repository_version_is_revalidated_after_lock_acquisition() {
        let d = tempdir().unwrap();
        let initialized = Forge::init(d.path()).unwrap();
        let root = initialized.root().to_path_buf();
        drop(initialized);

        // Model discovery by an old binary, followed by an exclusive updater
        // publishing a newer format before the old binary acquires its lock.
        validate_repo_version(&root).unwrap();
        let lock = acquire_cell_lock(&root, LockIntent::Shared).unwrap();
        fs::write(root.join("VERSION"), b"2\n").unwrap();

        let error = Forge::open_locked(root, lock, false)
            .err()
            .expect("locked open must revalidate VERSION");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
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

        assert!(acquire_cell_lock(&root, LockIntent::Exclusive).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }

    /// Read-only media rejects `O_RDWR` and `O_CREAT` outright, so opening
    /// `.forge/LOCK` that way made the documented read-only `fsck`/`verify`
    /// impossible before SQLite was ever reached. `flock` needs neither, so the
    /// access mode of the descriptor is the property to assert. Checking
    /// `F_GETFL` rather than filesystem permissions keeps the test meaningful
    /// when it runs as root, which bypasses permission bits.
    ///
    /// Only the loop-mount check in `readonly-verify` covers an actually
    /// read-only mount; this covers the open mode that made it impossible.
    #[cfg(unix)]
    #[test]
    fn shared_cell_locks_open_lock_read_only_and_never_create_it() {
        use std::os::fd::AsRawFd;

        let d = tempdir().unwrap();
        let initialized = Forge::init(d.path()).unwrap();
        let root = initialized.root().to_path_buf();
        drop(initialized);

        for intent in [LockIntent::Shared, LockIntent::ReadOnly] {
            let lock = acquire_cell_lock(&root, intent)
                .unwrap()
                .expect("an initialized repository has a LOCK to share");
            let flags = unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0, "F_GETFL failed for {intent:?}");
            assert_eq!(
                flags & libc::O_ACCMODE,
                libc::O_RDONLY,
                "{intent:?} must open .forge/LOCK read-only: read-only media \
                 refuses O_RDWR, which makes the repository unopenable"
            );
        }

        fs::remove_file(root.join("LOCK")).unwrap();
        assert!(
            acquire_cell_lock(&root, LockIntent::ReadOnly)
                .unwrap()
                .is_none(),
            "a read-only open tolerates media that cannot hand out a LOCK"
        );
        assert!(
            !root.join("LOCK").exists(),
            "no shared cell lock may create .forge/LOCK"
        );
    }
}
