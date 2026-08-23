use crate::metrics::TimingCounter;
use forge_core::now_ms;
use forge_types::{CasResult, Error, ObjectId, RefRow, Result};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS refs (
  name       TEXT PRIMARY KEY,
  oid        BLOB NOT NULL CHECK(length(oid)=32),
  kind       TEXT NOT NULL CHECK(kind IN ('commit','tree','conflict','snapshot')),
  protected  INTEGER NOT NULL DEFAULT 0,
  sealed     INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS reflog (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  old_oid BLOB,
  new_oid BLOB NOT NULL,
  agent_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  ts_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS reflog_name ON reflog(name, id);

CREATE TABLE IF NOT EXISTS namespaces (
  id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  pinned_oid BLOB,
  live_ref TEXT
);

CREATE TABLE IF NOT EXISTS observations (
  ns_id TEXT NOT NULL,
  mount TEXT NOT NULL,
  path  TEXT NOT NULL,
  oid   BLOB NOT NULL CHECK(length(oid)=32),
  PRIMARY KEY (ns_id, mount, path)
);

CREATE TABLE IF NOT EXISTS mounts (
  ns_id TEXT NOT NULL,
  path  TEXT NOT NULL,
  spec  TEXT NOT NULL,
  mode  TEXT NOT NULL CHECK(mode IN ('ro','rw')),
  PRIMARY KEY (ns_id, path)
);

CREATE TABLE IF NOT EXISTS overlay (
  ns_id    TEXT NOT NULL,
  mount    TEXT NOT NULL,
  path     TEXT NOT NULL,
  blob_oid BLOB,
  exec     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (ns_id, mount, path)
);

CREATE TABLE IF NOT EXISTS seals (
  tag        TEXT PRIMARY KEY,
  snap_oid   BLOB NOT NULL UNIQUE,
  commit_oid BLOB NOT NULL,
  tree_oid   BLOB NOT NULL,
  ts_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS landmarks (
  oid     BLOB PRIMARY KEY,
  kind    TEXT NOT NULL,
  reason  TEXT NOT NULL,
  ts_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS object_intro (
  oid        BLOB PRIMARY KEY,
  commit_oid BLOB NOT NULL,
  agent_id   TEXT NOT NULL,
  ts_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cap_root (
  id INTEGER PRIMARY KEY CHECK(id=1),
  hmac_key BLOB NOT NULL DEFAULT X'',
  seal_pub BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS schema_migrations (
  version INTEGER PRIMARY KEY,
  applied_ms INTEGER NOT NULL
);
"#;

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug)]
pub struct MountRow {
    pub path: String,
    pub spec: String,
    pub mode: String,
}

#[derive(Clone, Debug)]
pub struct OverlayRow {
    pub path: String,
    pub blob_oid: Option<ObjectId>,
    pub exec: bool,
}

#[derive(Clone, Debug)]
pub struct NsRow {
    pub id: String,
    pub agent_id: String,
    pub pinned_oid: Option<ObjectId>,
    pub live_ref: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ObservationRow {
    pub mount: String,
    pub path: String,
    pub oid: ObjectId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetaStats {
    /// Cumulative time spent in explicit `BEGIN IMMEDIATE` attempts, including
    /// their statements, COMMIT, or implicit rollback. Repository open and
    /// SQLite autocommit statements are not included.
    pub txn_us: u64,
    /// Number of explicit transaction attempts represented by `txn_us`.
    pub txn_count: u64,
    /// Cumulative time waiting to acquire ForgeFS's process-local SQLite
    /// connection mutex. This does not include SQLite's cross-process busy
    /// wait, which is part of `txn_us`.
    pub lock_wait_us: u64,
    /// Every acquisition of the process-local SQLite connection mutex,
    /// including reads and autocommit writes.
    pub lock_acquires: u64,
    pub busy: u64,
    pub cas_updated: u64,
    pub cas_forked: u64,
    pub cas_denied: u64,
}

impl MetaStats {
    /// Saturating sum over this process-lifetime snapshot: local mutex wait
    /// plus explicit transaction time. It is not a per-checkin measurement.
    pub fn sqlite_accounted_us(&self) -> u64 {
        self.lock_wait_us.saturating_add(self.txn_us)
    }
}

/// Effective SQLite settings that define the mutable catalog's durability
/// contract. These are verified during open and retained for diagnostics so a
/// benchmark can never compare runs with an implicit or weaker policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityPolicy {
    pub journal_mode: String,
    pub synchronous: i64,
    /// `None` means the platform does not support SQLite's macOS-only
    /// `F_FULLFSYNC` path.
    pub fullfsync: Option<bool>,
    /// True when this policy was only *observed* on a read-only open. Nothing
    /// was established or enforced: `journal_mode` is the on-disk mode, while
    /// `synchronous` has no on-disk representation at all and is this
    /// connection's effective value. A read-only catalog acknowledges no
    /// write, so it carries no durability contract to report.
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Debug, Default)]
struct MetaCounters {
    txn: TimingCounter,
    busy: AtomicU64,
    cas_updated: AtomicU64,
    cas_forked: AtomicU64,
    cas_denied: AtomicU64,
}

struct TxnTimer<'a> {
    started: Instant,
    timing: &'a TimingCounter,
    observed: bool,
}

impl TxnTimer<'_> {
    fn finish(mut self) {
        self.observe_once();
    }

    fn observe_once(&mut self) {
        if !self.observed {
            self.timing.observe(self.started.elapsed());
            self.observed = true;
        }
    }
}

impl Drop for TxnTimer<'_> {
    fn drop(&mut self) {
        self.observe_once();
    }
}

#[derive(Debug)]
struct TimedMutex<T> {
    inner: Mutex<T>,
    wait: TimingCounter,
}

impl<T> TimedMutex<T> {
    fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
            wait: TimingCounter::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, T> {
        let started = Instant::now();
        let guard = self.inner.lock();
        self.wait.observe(started.elapsed());
        guard
    }
}

pub struct Meta {
    write: TimedMutex<Connection>,
    stats: MetaCounters,
    durability: DurabilityPolicy,
    read_only: bool,
}

fn oid_from_blob(v: Vec<u8>) -> Result<ObjectId> {
    if v.len() != 32 {
        return Err(Error::Corrupt("oid blob length".into()));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(ObjectId(a))
}

/// Classify on SQLite's primary result code, never on its message text.
/// Matching prose meant SQLITE_LOCKED ("database table is locked") missed the
/// "database is locked" test and became exit 5, turning a retryable contention
/// into an unretryable internal failure, and left SQLITE_CONSTRAINT with
/// nowhere to go but Error::Sqlite -> exit 5 for a benign name clash.
fn map_sql(e: rusqlite::Error) -> Error {
    use rusqlite::ffi::ErrorCode;
    if let rusqlite::Error::SqliteFailure(inner, ref message) = e {
        let text = message.clone().unwrap_or_else(|| inner.to_string());
        return match inner.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::Busy(text),
            ErrorCode::ConstraintViolation => Error::Invalid(text),
            // A read-only open sets `query_only`, so SQLite itself refuses every
            // write on that connection. Report the refusal as denied authority
            // (exit 1) rather than as an internal SQLite fault (exit 5): the
            // caller asked a read-only handle to mutate the repository.
            ErrorCode::ReadOnly => Error::Denied(format!(
                "repository is open read-only; this operation writes metadata: {text}"
            )),
            _ => Error::Sqlite(text),
        };
    }
    Error::Sqlite(e.to_string())
}

/// Open a connection that cannot write, and prove it works before returning.
///
/// `SQLITE_OPEN_READONLY` never creates the file; `query_only` additionally
/// refuses writes to temp and attached databases, so every stray write path
/// fails as SQLITE_READONLY (`Error::Denied`) instead of reaching the media.
/// The probe query forces SQLite to open the database -- and in WAL mode its
/// shared-memory index -- now, while the failure can still be classified,
/// rather than inside some later unrelated query.
fn connect_read_only(path: &Path) -> std::result::Result<Connection, rusqlite::Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.pragma_update(None, "busy_timeout", 5000i64)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(conn)
}

fn cannot_open(error: &rusqlite::Error) -> bool {
    use rusqlite::ffi::ErrorCode;
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::CannotOpen | ErrorCode::ReadOnly)
    )
}

/// True when the filesystem holding `path` is mounted read-only. `statvfs`
/// answers this without writing, which a probe file could not.
#[cfg(unix)]
fn media_is_read_only(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a NUL-terminated C string and `buf` is a valid,
    // properly aligned allocation that `statvfs` fills in on success. Nothing
    // borrowed here escapes the call.
    unsafe {
        if libc::statvfs(c_path.as_ptr(), buf.as_mut_ptr()) != 0 {
            return false;
        }
        buf.assume_init().f_flag & libc::ST_RDONLY != 0
    }
}

#[cfg(not(unix))]
fn media_is_read_only(_path: &Path) -> bool {
    false
}

/// SQLite URI filenames are percent-decoded and `?`/`#` delimit the query, so
/// every byte outside the unreserved set must be escaped.
fn sqlite_uri(path: &Path, query: &str) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        Error::Invalid(format!(
            "metadata path is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    let mut uri = String::from("file:");
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(char::from(*byte));
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri.push('?');
    uri.push_str(query);
    Ok(uri)
}

/// A writable open on read-only media fails deep inside SQLite as
/// "unable to open database file", which is neither actionable nor an internal
/// fault. Name the actual cause, and classify it as denied authority (exit 1)
/// rather than as an internal SQLite failure (exit 5).
fn explain_read_only_media(path: &Path, error: Error) -> Error {
    if matches!(error, Error::Sqlite(_) | Error::Io(_) | Error::Denied(_))
        && media_is_read_only(path)
    {
        return Error::Denied(format!(
            "{} is on read-only media; opening a ForgeFS repository for writing needs writable media (`forge fsck` and `forge verify` run read-only)",
            path.display()
        ));
    }
    error
}

/// A read-only SQLite open fails with SQLITE_CANTOPEN when the catalog is
/// missing, and with SQLITE_CANTOPEN/SQLITE_READONLY_CANTINIT when a WAL
/// database still holds unrecovered frames and no shared-memory index can be
/// created on read-only media. Neither is an internal fault, so neither may
/// surface as the exit-5 `Sqlite` class.
fn map_read_only_open(path: &Path, e: rusqlite::Error) -> Error {
    use rusqlite::ffi::ErrorCode;
    if let rusqlite::Error::SqliteFailure(inner, _) = &e {
        if matches!(inner.code, ErrorCode::CannotOpen | ErrorCode::ReadOnly) {
            return Error::Invalid(format!(
                "cannot open {} read-only; either it is missing or a pending write-ahead log needs recovery on writable media first",
                path.display()
            ));
        }
    }
    map_sql(e)
}

fn schema_version(conn: &Connection) -> Result<i64> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .map_err(map_sql)?;
    if exists == 0 {
        return Ok(0);
    }
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )
    .map_err(map_sql)
}

fn migrate(conn: &mut Connection, from: i64) -> Result<()> {
    if from > CURRENT_SCHEMA_VERSION {
        return Err(Error::Invalid(format!(
            "metadata schema version {from} is newer than supported {CURRENT_SCHEMA_VERSION}"
        )));
    }
    if from == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    if from != 0 {
        return Err(Error::Invalid(format!(
            "unsupported metadata schema migration {from} -> {CURRENT_SCHEMA_VERSION}"
        )));
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql)?;
    tx.execute_batch(SCHEMA).map_err(map_sql)?;
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, ?2)",
        params![CURRENT_SCHEMA_VERSION, now_ms() as i64],
    )
    .map_err(map_sql)?;
    tx.commit().map_err(map_sql)
}

fn ref_exists(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool> {
    let found: i64 = tx
        .query_row("SELECT COUNT(*) FROM refs WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .map_err(map_sql)?;
    Ok(found != 0)
}

fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 512 || name.starts_with('/') || name.ends_with('/') {
        return Err(Error::Invalid(format!("invalid ref name {name:?}")));
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '\\' || c == ':')
    {
        return Err(Error::Invalid(format!("invalid ref name {name:?}")));
    }
    if name
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::Invalid(format!("invalid ref name {name:?}")));
    }
    Ok(())
}

fn validate_ref_kind(name: &str, kind: &str) -> Result<()> {
    validate_ref_name(name)?;
    if let Some(rest) = name.strip_prefix("inbox/") {
        let mut parts = rest.split('/');
        let agent = parts.next().unwrap_or_default();
        let id = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || agent.is_empty()
            || sanitize_agent(agent) != agent
            || ulid::Ulid::from_string(id).is_err()
        {
            return Err(Error::Invalid(format!("invalid inbox ref {name:?}")));
        }
    }
    let expected = if name == "main" || name.starts_with("heads/") {
        Some("commit")
    } else if name.starts_with("forks/inbox/") {
        Some("snapshot")
    } else if name.starts_with("forks/") {
        Some("commit")
    } else if name.starts_with("conflicts/") {
        Some("conflict")
    } else if name.starts_with("tags/") || name.starts_with("inbox/") {
        Some("snapshot")
    } else {
        None
    };
    if let Some(expected) = expected {
        if kind != expected {
            return Err(Error::Invalid(format!(
                "ref {name} requires kind {expected}, got {kind}"
            )));
        }
    }
    Ok(())
}

impl Meta {
    pub fn stats(&self) -> MetaStats {
        let txn = self.stats.txn.snapshot();
        let lock_wait = self.write.wait.snapshot();
        MetaStats {
            txn_us: txn.total_us,
            txn_count: txn.count,
            lock_wait_us: lock_wait.total_us,
            lock_acquires: lock_wait.count,
            busy: self.stats.busy.load(Ordering::Relaxed),
            cas_updated: self.stats.cas_updated.load(Ordering::Relaxed),
            cas_forked: self.stats.cas_forked.load(Ordering::Relaxed),
            cas_denied: self.stats.cas_denied.load(Ordering::Relaxed),
        }
    }

    pub fn durability_policy(&self) -> &DurabilityPolicy {
        &self.durability
    }

    /// True when this catalog was opened read-only. Every write path is
    /// refused by SQLite itself (`query_only`), so this is a diagnostic, not
    /// the enforcement point.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Complete a truncating WAL checkpoint through the same connection whose
    /// FULL durability policy was verified during `open`. SQLite reports a
    /// busy checkpoint in the result row rather than as an execution error, so
    /// treat partial completion as an explicit failure.
    pub fn checkpoint_truncate(&self) -> Result<CheckpointResult> {
        let conn = self.write.lock();
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|error| self.map_sql_counted(error))?;
        if busy != 0 || checkpointed_frames < log_frames {
            return Err(Error::Busy(format!(
                "WAL checkpoint incomplete: busy={busy} log={log_frames} checkpointed={checkpointed_frames}"
            )));
        }
        Ok(CheckpointResult {
            log_frames,
            checkpointed_frames,
        })
    }

    fn map_sql_counted(&self, error: rusqlite::Error) -> Error {
        let error = map_sql(error);
        if matches!(error, Error::Busy(_)) {
            self.stats.busy.fetch_add(1, Ordering::Relaxed);
        }
        error
    }

    fn begin_tx<'a>(&self, conn: &'a mut Connection) -> Result<rusqlite::Transaction<'a>> {
        conn.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| self.map_sql_counted(error))
    }

    fn txn_timer(&self) -> TxnTimer<'_> {
        TxnTimer {
            started: Instant::now(),
            timing: &self.stats.txn,
            observed: false,
        }
    }

    /// Connection-scoped settings that no read-only open needs to avoid: none
    /// of them writes to the database file or its directory.
    fn configure_connection(conn: &Connection) -> Result<()> {
        // Preserve the normal five-second contention policy without changing
        // an incompatible database on disk.
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(map_sql)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        Ok(())
    }

    /// Compatibility checks are read-only. Do not mutate a repository that
    /// this binary has already determined it cannot understand.
    fn compatible_schema_version(conn: &Connection) -> Result<i64> {
        let version = schema_version(conn)?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "metadata schema version {version} is newer than supported {CURRENT_SCHEMA_VERSION}"
            )));
        }
        Ok(version)
    }

    /// Report the settings actually in force. `read_only` marks the policy as
    /// observed rather than established, so a read-only open can never claim
    /// a WAL/FULL contract it did not set.
    fn observe_durability(conn: &Connection, read_only: bool) -> Result<DurabilityPolicy> {
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(map_sql)?;
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(map_sql)?;
        let fullfsync = {
            #[cfg(target_os = "macos")]
            {
                let fullfsync: i64 = conn
                    .pragma_query_value(None, "fullfsync", |row| row.get(0))
                    .map_err(map_sql)?;
                Some(fullfsync == 1)
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        };
        Ok(DurabilityPolicy {
            journal_mode: journal_mode.to_ascii_lowercase(),
            synchronous,
            fullfsync,
            read_only,
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_writable(path).map_err(|error| explain_read_only_media(path, error))
    }

    fn open_writable(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(map_sql)?;
        Self::configure_connection(&conn)?;
        let version = Self::compatible_schema_version(&conn)?;

        // Once compatible, establish the persistent durability contract before
        // any schema migration or metadata write.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sql)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(map_sql)?;
        #[cfg(target_os = "macos")]
        conn.pragma_update(None, "fullfsync", "ON")
            .map_err(map_sql)?;

        let durability = Self::observe_durability(&conn, false)?;
        if !durability.journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Corrupt(format!(
                "metadata durability requires journal_mode=WAL, got {}",
                durability.journal_mode
            )));
        }
        if durability.synchronous != 2 {
            return Err(Error::Corrupt(format!(
                "metadata durability requires synchronous=FULL(2), got {}",
                durability.synchronous
            )));
        }
        #[cfg(target_os = "macos")]
        if durability.fullfsync != Some(true) {
            return Err(Error::Corrupt(
                "metadata durability requires fullfsync=ON on macOS".into(),
            ));
        }

        migrate(&mut conn, version)?;
        conn.execute(
            "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
            [],
        )
        .map_err(map_sql)?;
        Ok(Self {
            write: TimedMutex::new(conn),
            stats: MetaCounters::default(),
            durability,
            read_only: false,
        })
    }

    /// Open the catalog without writing to it, to its journal, or to its
    /// directory: `SQLITE_OPEN_READONLY` (never `CREATE`), `query_only=1`, no
    /// durability pragma writes, no migration, and no `cap_root` scrub. This
    /// is the only open that works when the media itself is read-only.
    ///
    /// The durability pragmas are read and reported exactly as found. A
    /// read-only handle establishes no contract, so it must not fake one.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = match connect_read_only(path) {
            Ok(conn) => conn,
            // A WAL database is unreadable without its shared-memory index, and
            // read-only media cannot host one, so an ordinary read-only open of
            // a WAL catalog on such media fails with SQLITE_CANTOPEN.
            // `immutable=1` makes SQLite skip locking and the -shm entirely. It
            // asserts the file cannot change underneath this connection, which
            // is exactly what a read-only mount guarantees -- so it is used
            // only after confirming that mount, never as a blanket assumption.
            Err(error) if cannot_open(&error) && media_is_read_only(path) => {
                let uri = sqlite_uri(path, "immutable=1")?;
                connect_read_only(Path::new(&uri))
                    .map_err(|error| map_read_only_open(path, error))?
            }
            Err(error) => return Err(map_read_only_open(path, error)),
        };

        let version = Self::compatible_schema_version(&conn)?;
        if version < CURRENT_SCHEMA_VERSION {
            // Migration is a write. Say so, instead of failing later inside an
            // arbitrary query against a column this schema does not have.
            return Err(Error::Invalid(format!(
                "metadata schema version {version} needs migration to {CURRENT_SCHEMA_VERSION}, which a read-only open cannot perform"
            )));
        }
        let durability = Self::observe_durability(&conn, true)?;
        Ok(Self {
            write: TimedMutex::new(conn),
            stats: MetaCounters::default(),
            durability,
            read_only: true,
        })
    }

    pub fn set_cap_root(&self, seal_pub: &[u8]) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO cap_root (id, hmac_key, seal_pub) VALUES (1, X'', ?1)",
            params![seal_pub],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn get_seal_pub(&self) -> Result<Vec<u8>> {
        let conn = self.write.lock();
        conn.query_row("SELECT seal_pub FROM cap_root WHERE id=1", [], |r| r.get(0))
            .map_err(|_| Error::Corrupt("missing cap_root".into()))
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        let conn = self.write.lock();
        conn.query_row(
            "SELECT name, oid, kind, protected, sealed FROM refs WHERE name=?1",
            [name],
            |r| {
                let oid: Vec<u8> = r.get(1)?;
                Ok((
                    r.get::<_, String>(0)?,
                    oid,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql)?
        .map(|(name, oid, kind, p, s)| {
            Ok(RefRow {
                name,
                oid: oid_from_blob(oid)?,
                kind,
                protected: p != 0,
                sealed: s != 0,
            })
        })
        .transpose()
    }

    fn insert_intros_tx(
        tx: &rusqlite::Transaction<'_>,
        oids: &[ObjectId],
        commit: ObjectId,
        agent_id: &str,
        ts: i64,
    ) -> Result<()> {
        for oid in oids {
            tx.execute(
                "INSERT OR IGNORE INTO object_intro (oid, commit_oid, agent_id, ts_ms) VALUES (?1,?2,?3,?4)",
                params![oid.as_bytes().as_slice(), commit.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(map_sql)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_ref(
        &self,
        name: &str,
        oid: ObjectId,
        kind: &str,
        protected: bool,
        sealed: bool,
        agent_id: &str,
        reason: &str,
    ) -> Result<()> {
        validate_ref_kind(name, kind)?;
        if name.starts_with("tags/") {
            return Err(Error::Denied("tags may only be created by seal".into()));
        }
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let ts = now_ms() as i64;
        // Name the condition ourselves rather than leaking "UNIQUE constraint
        // failed: refs.name" to the caller.
        if ref_exists(&tx, name)? {
            return Err(Error::Invalid(format!("ref {name} already exists")));
        }
        tx.execute(
        "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,?4,?5,?6)",
        params![name, oid.as_bytes().as_slice(), kind, protected as i64, sealed as i64, ts],
    )
    .map_err(map_sql)?;
        tx.execute(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
        params![name, oid.as_bytes().as_slice(), agent_id, reason, ts],
    )
    .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_ref_with_intros(
        &self,
        name: &str,
        oid: ObjectId,
        kind: &str,
        protected: bool,
        sealed: bool,
        agent_id: &str,
        reason: &str,
        intro_oids: &[ObjectId],
    ) -> Result<()> {
        validate_ref_kind(name, kind)?;
        if name.starts_with("tags/") {
            return Err(Error::Denied("tags may only be created by seal".into()));
        }
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let ts = now_ms() as i64;
        tx.execute(
        "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,?4,?5,?6)",
        params![name, oid.as_bytes().as_slice(), kind, protected as i64, sealed as i64, ts],
    )
    .map_err(map_sql)?;
        tx.execute(
        "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
        params![name, oid.as_bytes().as_slice(), agent_id, reason, ts],
    )
    .map_err(map_sql)?;
        Self::insert_intros_tx(&tx, intro_oids, oid, agent_id, ts)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    pub fn update_mount_spec(&self, ns_id: &str, path: &str, spec: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "UPDATE mounts SET spec=?1 WHERE ns_id=?2 AND path=?3",
            params![spec, ns_id, path],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    /// Compare-and-swap. Protected refs (e.g. `main`) only move when
    /// `allow_protected` is true (merge/seal). Ordinary checkin/import never
    /// fork a protected name — they are denied.
    #[allow(clippy::too_many_arguments)]
    pub fn cas_ref(
        &self,
        name: &str,
        expected: ObjectId,
        new: ObjectId,
        kind: &str,
        agent_id: &str,
        fork_agent: &str,
        allow_protected: bool,
    ) -> Result<CasResult> {
        validate_ref_kind(name, kind)?;
        if name.starts_with("tags/") {
            self.stats.cas_denied.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Denied(
                "sealed tags cannot be updated through CAS".into(),
            ));
        }
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let row = tx
            .query_row(
                "SELECT oid, kind, protected, sealed FROM refs WHERE name=?1",
                [name],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.map_sql_counted(error))?;

        let ts = now_ms() as i64;

        if row.is_none() {
            tx.execute(
                "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,0,0,?4)",
                params![name, new.as_bytes().as_slice(), kind, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,NULL,?2,?3,'cas',?4)",
                params![name, new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            tx.commit().map_err(|error| self.map_sql_counted(error))?;
            txn_timer.finish();
            self.stats.cas_updated.fetch_add(1, Ordering::Relaxed);
            return Ok(CasResult::Updated {
                name: name.to_string(),
                oid: new,
            });
        }

        let (oid_b, current_kind, prot, sealed) = row.unwrap();
        if current_kind != kind {
            return Err(Error::Invalid(format!(
                "ref {name} kind is immutable: {current_kind} != {kind}"
            )));
        }
        let current = oid_from_blob(oid_b)?;
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if prot != 0 && !allow_protected {
            self.stats.cas_denied.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Denied(format!(
                "ref {name} is protected; only merge/seal may advance it"
            )));
        }

        let n = tx
            .execute(
                "UPDATE refs SET oid=?1, kind=?2, updated_ms=?3 WHERE name=?4 AND oid=?5 AND sealed=0",
                params![
                    new.as_bytes().as_slice(),
                    kind,
                    ts,
                    name,
                    expected.as_bytes().as_slice()
                ],
            )
            .map_err(|error| self.map_sql_counted(error))?;

        if n == 1 {
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'cas',?5)",
                params![
                    name,
                    expected.as_bytes().as_slice(),
                    new.as_bytes().as_slice(),
                    agent_id,
                    ts
                ],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            tx.commit().map_err(|error| self.map_sql_counted(error))?;
            txn_timer.finish();
            self.stats.cas_updated.fetch_add(1, Ordering::Relaxed);
            return Ok(CasResult::Updated {
                name: name.to_string(),
                oid: new,
            });
        }

        // Lost CAS → fork.
        let fork = format!(
            "forks/{}/{}/{}",
            name,
            sanitize_agent(fork_agent),
            ulid::Ulid::new()
        );
        validate_ref_kind(&fork, kind)?;
        tx.execute(
            "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,0,0,?4)",
            params![fork, new.as_bytes().as_slice(), kind, ts],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'fork',?5)",
            params![
                fork,
                current.as_bytes().as_slice(),
                new.as_bytes().as_slice(),
                agent_id,
                ts
            ],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        tx.commit().map_err(|error| self.map_sql_counted(error))?;
        txn_timer.finish();
        self.stats.cas_forked.fetch_add(1, Ordering::Relaxed);
        Ok(CasResult::Forked {
            requested: name.to_string(),
            fork,
            ours: new,
            theirs: current,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cas_ref_with_intros(
        &self,
        name: &str,
        expected: ObjectId,
        new: ObjectId,
        kind: &str,
        agent_id: &str,
        fork_agent: &str,
        allow_protected: bool,
        intro_oids: &[ObjectId],
    ) -> Result<CasResult> {
        validate_ref_kind(name, kind)?;
        if name.starts_with("tags/") {
            self.stats.cas_denied.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Denied(
                "sealed tags cannot be updated through CAS".into(),
            ));
        }
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let row = tx
            .query_row(
                "SELECT oid, kind, protected, sealed FROM refs WHERE name=?1",
                [name],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.map_sql_counted(error))?;

        let ts = now_ms() as i64;

        if row.is_none() {
            tx.execute(
                "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,0,0,?4)",
                params![name, new.as_bytes().as_slice(), kind, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,NULL,?2,?3,'cas',?4)",
                params![name, new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;
            tx.commit().map_err(|error| self.map_sql_counted(error))?;
            txn_timer.finish();
            self.stats.cas_updated.fetch_add(1, Ordering::Relaxed);
            return Ok(CasResult::Updated {
                name: name.to_string(),
                oid: new,
            });
        }

        let (oid_b, current_kind, prot, sealed) = row.unwrap();
        if current_kind != kind {
            return Err(Error::Invalid(format!(
                "ref {name} kind is immutable: {current_kind} != {kind}"
            )));
        }
        let current = oid_from_blob(oid_b)?;
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if prot != 0 && !allow_protected {
            self.stats.cas_denied.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Denied(format!(
                "ref {name} is protected; only merge/seal may advance it"
            )));
        }

        let n = tx
            .execute(
                "UPDATE refs SET oid=?1, kind=?2, updated_ms=?3 WHERE name=?4 AND oid=?5 AND sealed=0",
                params![
                    new.as_bytes().as_slice(),
                    kind,
                    ts,
                    name,
                    expected.as_bytes().as_slice()
                ],
            )
            .map_err(|error| self.map_sql_counted(error))?;

        if n == 1 {
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'cas',?5)",
                params![
                    name,
                    expected.as_bytes().as_slice(),
                    new.as_bytes().as_slice(),
                    agent_id,
                    ts
                ],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;
            tx.commit().map_err(|error| self.map_sql_counted(error))?;
            txn_timer.finish();
            self.stats.cas_updated.fetch_add(1, Ordering::Relaxed);
            return Ok(CasResult::Updated {
                name: name.to_string(),
                oid: new,
            });
        }

        // Lost CAS → fork.
        let fork = format!(
            "forks/{}/{}/{}",
            name,
            sanitize_agent(fork_agent),
            ulid::Ulid::new()
        );
        validate_ref_kind(&fork, kind)?;
        tx.execute(
            "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,?3,0,0,?4)",
            params![fork, new.as_bytes().as_slice(), kind, ts],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'fork',?5)",
            params![
                fork,
                current.as_bytes().as_slice(),
                new.as_bytes().as_slice(),
                agent_id,
                ts
            ],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;
        tx.commit().map_err(|error| self.map_sql_counted(error))?;
        txn_timer.finish();
        self.stats.cas_forked.fetch_add(1, Ordering::Relaxed);
        Ok(CasResult::Forked {
            requested: name.to_string(),
            fork,
            ours: new,
            theirs: current,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cas_ref_session(
        &self,
        name: &str,
        expected: ObjectId,
        new: ObjectId,
        agent_id: &str,
        fork_agent: &str,
        ns_id: &str,
        mount_path: &str,
        intro_oids: &[ObjectId],
    ) -> Result<CasResult> {
        validate_ref_kind(name, "commit")?;
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let row = tx
            .query_row(
                "SELECT oid, kind, protected, sealed FROM refs WHERE name=?1",
                [name],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.map_sql_counted(error))?
            .ok_or_else(|| Error::NotFound(format!("ref {name}")))?;
        let (oid_b, kind, protected, sealed) = row;
        if kind != "commit" {
            return Err(Error::Invalid(format!("ref {name} is {kind}, not commit")));
        }
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if protected != 0 {
            self.stats.cas_denied.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Denied(format!(
                "ref {name} is protected; session checkin cannot advance it"
            )));
        }
        let current = oid_from_blob(oid_b)?;
        let ts = now_ms() as i64;

        let result = if current == expected {
            let n = tx
                .execute(
                    "UPDATE refs SET oid=?1, updated_ms=?2 WHERE name=?3 AND oid=?4 AND kind='commit' AND sealed=0 AND protected=0",
                    params![new.as_bytes().as_slice(), ts, name, expected.as_bytes().as_slice()],
                )
                .map_err(|error| self.map_sql_counted(error))?;
            if n != 1 {
                self.stats.busy.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Busy(format!("ref {name} changed during checkin")));
            }
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'cas',?5)",
                params![name, expected.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            CasResult::Updated {
                name: name.to_string(),
                oid: new,
            }
        } else {
            let fork = format!(
                "forks/{}/{}/{}",
                name,
                sanitize_agent(fork_agent),
                ulid::Ulid::new()
            );
            validate_ref_kind(&fork, "commit")?;
            tx.execute(
                "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,'commit',0,0,?3)",
                params![fork, new.as_bytes().as_slice(), ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            tx.execute(
                "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,'fork',?5)",
                params![fork, current.as_bytes().as_slice(), new.as_bytes().as_slice(), agent_id, ts],
            )
            .map_err(|error| self.map_sql_counted(error))?;
            let root_spec = format!("ref:{fork}");
            let n = tx
                .execute(
                    "UPDATE mounts SET spec=?1 WHERE ns_id=?2 AND path=?3",
                    params![root_spec, ns_id, mount_path],
                )
                .map_err(|error| self.map_sql_counted(error))?;
            if n != 1 {
                return Err(Error::Corrupt(format!(
                    "missing checkin mount {ns_id}:{mount_path}"
                )));
            }
            CasResult::Forked {
                requested: name.to_string(),
                fork,
                ours: new,
                theirs: current,
            }
        };

        tx.execute(
            "DELETE FROM overlay WHERE ns_id=?1 AND mount=?2",
            params![ns_id, mount_path],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        let n = tx
            .execute(
                "UPDATE namespaces SET pinned_oid=?1 WHERE id=?2",
                params![new.as_bytes().as_slice(), ns_id],
            )
            .map_err(|error| self.map_sql_counted(error))?;
        if n != 1 {
            return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
        }
        tx.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(|error| self.map_sql_counted(error))?;
        Self::insert_intros_tx(&tx, intro_oids, new, agent_id, ts)?;
        tx.commit().map_err(|error| self.map_sql_counted(error))?;
        txn_timer.finish();
        match &result {
            CasResult::Updated { .. } => {
                self.stats.cas_updated.fetch_add(1, Ordering::Relaxed);
            }
            CasResult::Forked { .. } => {
                self.stats.cas_forked.fetch_add(1, Ordering::Relaxed);
            }
            CasResult::Noop { .. } => {}
        }
        Ok(result)
    }

    pub fn complete_noop_session(
        &self,
        ns_id: &str,
        mount_path: &str,
        pinned: ObjectId,
    ) -> Result<()> {
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        tx.execute(
            "DELETE FROM overlay WHERE ns_id=?1 AND mount=?2",
            params![ns_id, mount_path],
        )
        .map_err(map_sql)?;
        let n = tx
            .execute(
                "UPDATE namespaces SET pinned_oid=?1 WHERE id=?2",
                params![pinned.as_bytes().as_slice(), ns_id],
            )
            .map_err(map_sql)?;
        if n != 1 {
            return Err(Error::Corrupt(format!("missing namespace {ns_id}")));
        }
        tx.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    pub fn list_refs(&self) -> Result<Vec<RefRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare("SELECT name, oid, kind, protected, sealed FROM refs ORDER BY name")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (name, oid, kind, p, s) = row.map_err(map_sql)?;
            out.push(RefRow {
                name,
                oid: oid_from_blob(oid)?,
                kind,
                protected: p != 0,
                sealed: s != 0,
            });
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        id: &str,
        agent_id: &str,
        pinned: ObjectId,
        live_ref: &str,
        mount_main: bool,
    ) -> Result<()> {
        validate_ref_kind(live_ref, "commit")?;
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let ts = now_ms() as i64;
        tx.execute(
            "INSERT INTO namespaces (id, agent_id, created_ms, pinned_oid, live_ref) VALUES (?1,?2,?3,?4,?5)",
            params![id, agent_id, ts, pinned.as_bytes().as_slice(), live_ref],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,'commit',0,0,?3)",
            params![live_ref, pinned.as_bytes().as_slice(), ts],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,NULL,?2,?3,'session',?4)",
            params![live_ref, pinned.as_bytes().as_slice(), agent_id, ts],
        )
        .map_err(map_sql)?;
        let root_spec = format!("ref:{live_ref}");
        tx.execute(
            "INSERT INTO mounts (ns_id, path, spec, mode) VALUES (?1,'/',?2,'rw')",
            params![id, root_spec],
        )
        .map_err(map_sql)?;
        if mount_main {
            tx.execute(
                "INSERT INTO mounts (ns_id, path, spec, mode) VALUES (?1,'/main','ref:main','ro')",
                [id],
            )
            .map_err(map_sql)?;
        }
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    pub fn list_namespaces(&self) -> Result<Vec<NsRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare("SELECT id, agent_id, pinned_oid, live_ref FROM namespaces ORDER BY id")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, agent_id, pinned_oid, live_ref) = row.map_err(map_sql)?;
            out.push(NsRow {
                id,
                agent_id,
                pinned_oid: pinned_oid.map(oid_from_blob).transpose()?,
                live_ref,
            });
        }
        Ok(out)
    }

    pub fn insert_namespace(
        &self,
        id: &str,
        agent_id: &str,
        pinned: ObjectId,
        live_ref: &str,
    ) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT INTO namespaces (id, agent_id, created_ms, pinned_oid, live_ref) VALUES (?1,?2,?3,?4,?5)",
            params![
                id,
                agent_id,
                now_ms() as i64,
                pinned.as_bytes().as_slice(),
                live_ref
            ],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn get_namespace(&self, id: &str) -> Result<NsRow> {
        let conn = self.write.lock();
        conn.query_row(
            "SELECT id, agent_id, pinned_oid, live_ref FROM namespaces WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql)?
        .ok_or_else(|| Error::NotFound(format!("namespace {id}")))
        .and_then(|(id, agent_id, pin, live_ref)| {
            Ok(NsRow {
                id,
                agent_id,
                pinned_oid: pin.map(oid_from_blob).transpose()?,
                live_ref,
            })
        })
    }

    pub fn set_pin(&self, ns_id: &str, oid: ObjectId) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "UPDATE namespaces SET pinned_oid=?1 WHERE id=?2",
            params![oid.as_bytes().as_slice(), ns_id],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn observe(&self, ns_id: &str, mount: &str, path: &str, oid: ObjectId) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO observations (ns_id, mount, path, oid) VALUES (?1,?2,?3,?4)",
            params![ns_id, mount, path, oid.as_bytes().as_slice()],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn observations(&self, ns_id: &str) -> Result<Vec<ObservationRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare("SELECT mount, path, oid FROM observations WHERE ns_id=?1")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([ns_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (mount, path, oid) = row.map_err(map_sql)?;
            out.push(ObservationRow {
                mount,
                path,
                oid: oid_from_blob(oid)?,
            });
        }
        Ok(out)
    }

    pub fn observations_clear(&self, ns_id: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(map_sql)?;
        Ok(())
    }

    pub fn insert_mount(&self, ns_id: &str, path: &str, spec: &str, mode: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO mounts (ns_id, path, spec, mode) VALUES (?1,?2,?3,?4)",
            params![ns_id, path, spec, mode],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn list_mounts(&self, ns_id: &str) -> Result<Vec<MountRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare("SELECT path, spec, mode FROM mounts WHERE ns_id=?1")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([ns_id], |r| {
                Ok(MountRow {
                    path: r.get(0)?,
                    spec: r.get(1)?,
                    mode: r.get(2)?,
                })
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_sql)?);
        }
        Ok(out)
    }

    pub fn overlay_upsert(
        &self,
        ns_id: &str,
        mount: &str,
        path: &str,
        blob_oid: Option<ObjectId>,
        exec: bool,
    ) -> Result<()> {
        let conn = self.write.lock();
        let oid = blob_oid.map(|o| o.0.to_vec());
        conn.execute(
            "INSERT OR REPLACE INTO overlay (ns_id, mount, path, blob_oid, exec) VALUES (?1,?2,?3,?4,?5)",
            params![ns_id, mount, path, oid, exec as i64],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn overlay_list(&self, ns_id: &str, mount: &str) -> Result<Vec<OverlayRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare("SELECT path, blob_oid, exec FROM overlay WHERE ns_id=?1 AND mount=?2")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map(params![ns_id, mount], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<Vec<u8>>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (path, oid, exec) = row.map_err(map_sql)?;
            out.push(OverlayRow {
                path,
                blob_oid: match oid {
                    Some(b) => Some(oid_from_blob(b)?),
                    None => None,
                },
                exec: exec != 0,
            });
        }
        Ok(out)
    }

    pub fn overlay_clear(&self, ns_id: &str, mount: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "DELETE FROM overlay WHERE ns_id=?1 AND mount=?2",
            params![ns_id, mount],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn intro_insert(&self, oid: ObjectId, commit: ObjectId, agent_id: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR IGNORE INTO object_intro (oid, commit_oid, agent_id, ts_ms) VALUES (?1,?2,?3,?4)",
            params![
                oid.as_bytes().as_slice(),
                commit.as_bytes().as_slice(),
                agent_id,
                now_ms() as i64
            ],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn intro_insert_many(
        &self,
        oids: &[ObjectId],
        commit: ObjectId,
        agent_id: &str,
    ) -> Result<()> {
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let ts = now_ms() as i64;
        Self::insert_intros_tx(&tx, oids, commit, agent_id, ts)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }
    pub fn intro_get(&self, oid: ObjectId) -> Result<Option<String>> {
        let conn = self.write.lock();
        conn.query_row(
            "SELECT agent_id FROM object_intro WHERE oid=?1",
            params![oid.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql)
    }

    pub fn landmark(&self, oid: ObjectId, kind: &str, reason: &str) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR IGNORE INTO landmarks (oid, kind, reason, ts_ms) VALUES (?1,?2,?3,?4)",
            params![oid.as_bytes().as_slice(), kind, reason, now_ms() as i64],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn insert_seal(
        &self,
        tag: &str,
        snap: ObjectId,
        commit: ObjectId,
        tree: ObjectId,
    ) -> Result<()> {
        self.commit_seal(tag, snap, commit, tree, "seal")
    }

    /// Atomically publish a sealed tag: refs row + seals row + landmarks.
    pub fn commit_seal(
        &self,
        tag: &str,
        snap: ObjectId,
        commit: ObjectId,
        tree: ObjectId,
        agent_id: &str,
    ) -> Result<()> {
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let ts = now_ms() as i64;
        let tag_ref = format!("tags/{tag}");
        validate_ref_kind(&tag_ref, "snapshot")?;
        // A frozen tag is sealed state, so re-sealing must surface as
        // Error::Sealed (exit 2), not as a PRIMARY KEY violation.
        if ref_exists(&tx, &tag_ref)? {
            return Err(Error::Sealed(tag_ref));
        }
        tx.execute(
            "INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES (?1,?2,'snapshot',1,1,?3)",
            params![tag_ref, snap.as_bytes().as_slice(), ts],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1, NULL, ?2, ?3, 'seal', ?4)",
            params![tag_ref, snap.as_bytes().as_slice(), agent_id, ts],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO seals (tag, snap_oid, commit_oid, tree_oid, ts_ms) VALUES (?1,?2,?3,?4,?5)",
            params![
                tag,
                snap.as_bytes().as_slice(),
                commit.as_bytes().as_slice(),
                tree.as_bytes().as_slice(),
                ts
            ],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT OR IGNORE INTO landmarks (oid, kind, reason, ts_ms) VALUES (?1,'snapshot','seal',?2)",
            params![snap.as_bytes().as_slice(), ts],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT OR IGNORE INTO landmarks (oid, kind, reason, ts_ms) VALUES (?1,'commit','seal',?2)",
            params![commit.as_bytes().as_slice(), ts],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    pub fn get_seal(&self, tag: &str) -> Result<Option<(ObjectId, ObjectId, ObjectId)>> {
        let conn = self.write.lock();
        conn.query_row(
            "SELECT snap_oid, commit_oid, tree_oid FROM seals WHERE tag=?1",
            [tag],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql)?
        .map(|(a, b, c)| Ok((oid_from_blob(a)?, oid_from_blob(b)?, oid_from_blob(c)?)))
        .transpose()
    }

    #[allow(clippy::type_complexity)]
    pub fn reflog(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Vec<(Option<ObjectId>, ObjectId, String, String)>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare(
                "SELECT old_oid, new_oid, agent_id, reason FROM reflog WHERE name=?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(map_sql)?;
        let rows = stmt
            .query_map(params![name, limit as i64], |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (old, new, agent, reason) = row.map_err(map_sql)?;
            out.push((
                old.map(oid_from_blob).transpose()?,
                oid_from_blob(new)?,
                agent,
                reason,
            ));
        }
        Ok(out)
    }
}

pub fn sanitize_agent(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if t.is_empty() {
        "anon".into()
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn oid(n: u8) -> ObjectId {
        ObjectId([n; 32])
    }

    #[test]
    fn cas_two_threads_one_fork() {
        let d = tempdir().unwrap();
        let meta = Arc::new(Meta::open(&d.path().join("m.sqlite")).unwrap());
        meta.insert_ref("shared", oid(1), "commit", false, false, "init", "init")
            .unwrap();
        let m1 = meta.clone();
        let m2 = meta.clone();
        let h1 =
            thread::spawn(move || m1.cas_ref("shared", oid(1), oid(2), "commit", "a", "a", false));
        let h2 =
            thread::spawn(move || m2.cas_ref("shared", oid(1), oid(3), "commit", "b", "b", false));
        let r1 = h1.join().unwrap().unwrap();
        let r2 = h2.join().unwrap().unwrap();
        let results = [r1, r2];
        let updates = results
            .iter()
            .filter(|r| matches!(r, CasResult::Updated { .. }))
            .count();
        let forks = results
            .iter()
            .filter(|r| matches!(r, CasResult::Forked { .. }))
            .count();
        assert_eq!(updates, 1, "{results:?}");
        assert_eq!(forks, 1, "{results:?}");
    }

    #[test]
    fn protected_ref_cannot_cas_without_flag() {
        let d = tempdir().unwrap();
        let meta = Meta::open(&d.path().join("m.sqlite")).unwrap();
        meta.insert_ref("main", oid(1), "commit", true, false, "init", "init")
            .unwrap();
        let err = meta
            .cas_ref("main", oid(1), oid(2), "commit", "a", "a", false)
            .unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
        let ok = meta
            .cas_ref("main", oid(1), oid(2), "commit", "a", "a", true)
            .unwrap();
        assert!(matches!(ok, CasResult::Updated { .. }));
    }
}

#[cfg(test)]
mod durability_policy_tests {
    use super::*;
    use tempfile::tempdir;

    /// A read-only open must read the catalog, refuse every write with a clear
    /// denial, and report the durability policy it *found* rather than the
    /// WAL/FULL contract it never established.
    #[test]
    fn read_only_open_reads_refuses_writes_and_reports_observed_policy() {
        let d = tempdir().unwrap();
        let path = d.path().join("meta.sqlite");
        {
            let writable = Meta::open(&path).unwrap();
            writable.set_cap_root(b"0123456789abcdef").unwrap();
            assert!(!writable.read_only());
            assert!(!writable.durability_policy().read_only);
        }

        let ro = Meta::open_read_only(&path).unwrap();
        assert!(ro.read_only());
        assert_eq!(ro.get_seal_pub().unwrap(), b"0123456789abcdef".to_vec());
        // journal_mode is persisted in the database header, so it is honest to
        // report it; the policy is flagged as observed, not established.
        assert_eq!(ro.durability_policy().journal_mode, "wal");
        assert!(ro.durability_policy().read_only);

        let denied = ro
            .set_cap_root(b"fedcba9876543210")
            .expect_err("query_only must refuse a metadata write");
        assert!(matches!(denied, Error::Denied(_)), "{denied}");
    }

    /// `SQLITE_OPEN_READONLY` never creates the catalog, and the failure is
    /// classified as input (exit 1), never as an internal SQLite fault.
    #[test]
    fn read_only_open_never_creates_the_catalog() {
        let d = tempdir().unwrap();
        let path = d.path().join("absent.sqlite");
        let Err(error) = Meta::open_read_only(&path) else {
            panic!("a read-only open must not create the database");
        };
        assert!(matches!(error, Error::Invalid(_)), "{error}");
        assert!(
            !path.exists(),
            "a read-only open must not create meta.sqlite"
        );
    }

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
        assert_eq!(meta.durability_policy().journal_mode, "wal");
        assert_eq!(meta.durability_policy().synchronous, 2);
        #[cfg(target_os = "macos")]
        {
            let fullfsync: i64 = conn
                .pragma_query_value(None, "fullfsync", |row| row.get(0))
                .unwrap();
            assert_eq!(fullfsync, 1);
            assert_eq!(meta.durability_policy().fullfsync, Some(true));
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(meta.durability_policy().fullfsync, None);
    }
}
