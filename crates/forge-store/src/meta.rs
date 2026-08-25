use crate::metrics::TimingCounter;
use forge_core::now_ms;
use forge_types::{CasResult, Error, ObjectId, ObjectType, RefRow, Result};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// The durable record `abandon_ref` leaves behind for a retired ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredRef {
    pub name: String,
    pub oid: ObjectId,
    pub kind: String,
    pub agent_id: String,
    pub ts_ms: i64,
}

/// What `abandon_session` removed from the root set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbandonedSession {
    pub ns_id: String,
    pub discarded_overlay: usize,
    pub removed_mounts: usize,
    pub removed_observations: usize,
}

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
  kind  TEXT NOT NULL,
  oid   BLOB,
  PRIMARY KEY (ns_id, mount, path),
  CHECK((kind='absent' AND oid IS NULL)
        OR (kind IN ('blob','tree') AND length(oid)=32))
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

pub const CURRENT_SCHEMA_VERSION: i64 = 2;

/// Reflog `reason` that retires a ref name for good. See `ref_retired`.
pub const REFLOG_ABANDON: &str = "abandon";

/// The only ref namespace `abandon_ref` will retire.
///
/// A fork name is `forks/<ref>/<agent>/<ulid>`: it is minted once by a losing
/// CAS and never recomputed, so retiring it cannot collide with a later ref of
/// the same name. Every other namespace -- `main`, `heads/`, `tags/`,
/// `conflicts/`, `inbox/` -- is published history, a live session head, or
/// sealed, and none of them is the unbounded steady-state growth this verb
/// exists to bound.
pub const ABANDONABLE_PREFIX: &str = "forks/";

const REFS_COLUMNS: &[&str] = &["name", "oid", "kind", "protected", "sealed", "updated_ms"];
const REFS_VALUES: &str = "typeof(name)='text' AND typeof(oid)='blob' \
    AND typeof(kind)='text' AND typeof(protected)='integer' AND protected IN (0,1) \
    AND typeof(sealed)='integer' AND sealed IN (0,1) AND typeof(updated_ms)='integer'";
const REFLOG_COLUMNS: &[&str] = &[
    "id", "name", "old_oid", "new_oid", "agent_id", "reason", "ts_ms",
];
const REFLOG_VALUES: &str = "typeof(id)='integer' AND typeof(name)='text' \
    AND typeof(old_oid) IN ('null','blob') AND typeof(new_oid)='blob' \
    AND typeof(agent_id)='text' AND typeof(reason)='text' AND typeof(ts_ms)='integer'";
const NAMESPACE_COLUMNS: &[&str] = &["id", "agent_id", "created_ms", "pinned_oid", "live_ref"];
const NAMESPACE_VALUES: &str = "typeof(id)='text' AND typeof(agent_id)='text' \
    AND typeof(created_ms)='integer' AND typeof(pinned_oid) IN ('null','blob') \
    AND typeof(live_ref) IN ('null','text')";
const OBSERVATION_COLUMNS: &[&str] = &["ns_id", "mount", "path", "kind", "oid"];
const OBSERVATION_VALUES: &str = "typeof(ns_id)='text' AND typeof(mount)='text' \
    AND typeof(path)='text' \
    AND ((kind='absent' AND oid IS NULL) \
         OR (kind IN ('blob','tree') AND typeof(oid)='blob'))";
const MOUNT_COLUMNS: &[&str] = &["ns_id", "path", "spec", "mode"];
const MOUNT_VALUES: &str = "typeof(ns_id)='text' AND typeof(path)='text' \
    AND typeof(spec)='text' AND typeof(mode)='text' AND mode IN ('ro','rw')";
const OVERLAY_COLUMNS: &[&str] = &["ns_id", "mount", "path", "blob_oid", "exec"];
const OVERLAY_VALUES: &str = "typeof(ns_id)='text' AND typeof(mount)='text' \
    AND typeof(path)='text' AND typeof(blob_oid) IN ('null','blob') \
    AND typeof(exec)='integer' AND exec IN (0,1)";
const SEAL_COLUMNS: &[&str] = &["tag", "snap_oid", "commit_oid", "tree_oid", "ts_ms"];
const SEAL_VALUES: &str = "typeof(tag)='text' AND typeof(snap_oid)='blob' \
    AND typeof(commit_oid)='blob' AND typeof(tree_oid)='blob' AND typeof(ts_ms)='integer'";
const LANDMARK_COLUMNS: &[&str] = &["oid", "kind", "reason", "ts_ms"];
const LANDMARK_VALUES: &str = "typeof(oid)='blob' AND typeof(kind)='text' \
    AND typeof(reason)='text' AND typeof(ts_ms)='integer'";
const INTRO_COLUMNS: &[&str] = &["oid", "commit_oid", "agent_id", "ts_ms"];
const INTRO_VALUES: &str = "typeof(oid)='blob' AND typeof(commit_oid)='blob' \
    AND typeof(agent_id)='text' AND typeof(ts_ms)='integer'";
const CAP_ROOT_COLUMNS: &[&str] = &["id", "hmac_key", "seal_pub"];
const CAP_ROOT_VALUES: &str = "typeof(id)='integer' AND id=1 AND typeof(hmac_key)='blob' \
    AND length(hmac_key)=0 AND typeof(seal_pub)='blob'";
const MIGRATION_COLUMNS: &[&str] = &["version", "applied_ms"];
const MIGRATION_VALUES: &str = "typeof(version)='integer' AND typeof(applied_ms)='integer'";

/// Every relation the current schema owns, paired with the exact column list
/// this binary requires. One definition governs both the catalog audit and the
/// post-migration shape check, so the two can never disagree about what
/// "current" means.
const CATALOG_TABLES: &[(&str, &[&str])] = &[
    ("refs", REFS_COLUMNS),
    ("reflog", REFLOG_COLUMNS),
    ("namespaces", NAMESPACE_COLUMNS),
    ("observations", OBSERVATION_COLUMNS),
    ("mounts", MOUNT_COLUMNS),
    ("overlay", OVERLAY_COLUMNS),
    ("seals", SEAL_COLUMNS),
    ("landmarks", LANDMARK_COLUMNS),
    ("object_intro", INTRO_COLUMNS),
    ("cap_root", CAP_ROOT_COLUMNS),
    ("schema_migrations", MIGRATION_COLUMNS),
];

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

/// What a session actually saw at one path. I9 says reads record what was
/// read; the two cases that used to record nothing -- a directory listing and a
/// lookup that resolved to nothing -- are first-class variants here so that
/// checkin can detect write skew against them.
///
/// `Tree` stores the canonical tree object id rather than a separate digest of
/// the listing: a forge tree *is* the canonical encoding of exactly the
/// (name, kind, oid, exec) tuples a directory read returns, so its id is a
/// faithful, already-content-addressed digest of what the caller saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// A file with this content id.
    Blob(ObjectId),
    /// A directory whose canonical tree object had this id.
    Tree(ObjectId),
    /// Nothing resolved at this path.
    Absent,
}

impl Observed {
    /// The stored discriminant. Unknown strings fail closed on read.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Observed::Blob(_) => "blob",
            Observed::Tree(_) => "tree",
            Observed::Absent => "absent",
        }
    }

    /// The object this observation names, if it names one at all.
    #[must_use]
    pub fn oid(&self) -> Option<ObjectId> {
        match self {
            Observed::Blob(id) | Observed::Tree(id) => Some(*id),
            Observed::Absent => None,
        }
    }

    /// How the observation is rendered in a `StaleObservation` error.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Observed::Blob(id) => id.hex(),
            Observed::Tree(id) => format!("tree {}", id.hex()),
            Observed::Absent => "absent".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObservationRow {
    pub mount: String,
    pub path: String,
    pub seen: Observed,
}

/// One deterministic defect in the mutable SQLite catalog. The API layer maps
/// these directly into fsck findings; keeping the checks here gives the
/// catalog a single invariant owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogFinding {
    pub code: String,
    pub resource: String,
    pub detail: String,
}

/// Type constraint for an object ID held by the mutable catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogObjectExpectation {
    Any,
    Exact(ObjectType),
    /// `object_intro.oid` records values introduced by a tree transition, so
    /// only tree and blob objects are valid there.
    TreeEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogObjectRoot {
    pub oid: ObjectId,
    pub expected: CatalogObjectExpectation,
    pub resource: String,
}

/// Valid rows captured by the catalog audit's single read transaction. Full
/// fsck consumes these instead of re-reading mutable tables through strict
/// production accessors after the snapshot has ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogNamespaceRow {
    pub id: String,
    pub pinned_oid: Option<ObjectId>,
    pub live_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogMountRow {
    pub ns_id: String,
    pub path: String,
    pub spec: String,
    pub mode: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealRow {
    pub tag: String,
    pub snap_oid: ObjectId,
    pub commit_oid: ObjectId,
    pub tree_oid: ObjectId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogAudit {
    pub findings: Vec<CatalogFinding>,
    pub roots: Vec<CatalogObjectRoot>,
    pub seals: Vec<SealRow>,
    pub refs: Vec<RefRow>,
    /// Every refs row was decoded, including its object ID. Consumers may
    /// diagnose absent ref targets only when this is true; otherwise absence
    /// from `refs` means "unreadable row", not "missing relation".
    pub refs_complete: bool,
    pub namespaces: Vec<CatalogNamespaceRow>,
    pub mounts: Vec<CatalogMountRow>,
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
    /// Checkins whose overlay reproduced the pinned tree on an unmoved ref, so
    /// no commit was published and no ref CAS was attempted.
    pub cas_noop: u64,
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
    cas_noop: AtomicU64,
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

/// Connections that may only SELECT.
///
/// WAL exists so readers do not have to queue behind the writer, but every
/// catalog query used to take the one connection mutex, so three of the four
/// acquisitions a `Forge::read` makes were pure reads waiting on writes
/// (issue #308). The pool takes them off that mutex.
///
/// Members are opened by `connect_read_only`, the same constructor the
/// read-only-media open uses: `SQLITE_OPEN_READONLY` plus `query_only=1`, and
/// no `journal_mode` or `synchronous` pragma, so a pool member cannot mutate
/// the database or its durability contract even by accident.
///
/// Slots fill lazily. A process-per-command CLI opens at most one extra
/// connection and only if it reads at all; a threaded server grows to
/// `slots.len()` as readers actually collide. The pool is an optimisation and
/// never a failure mode: if a member cannot be opened, the caller falls back
/// to the write connection.
struct ReadPool {
    path: PathBuf,
    slots: Vec<Mutex<Option<Connection>>>,
    next: AtomicUsize,
    wait: TimingCounter,
}

/// Read connections per catalog. Small on purpose: each one is an open file
/// descriptor and a mapped wal-index region, and readers past this many are
/// better off queueing than multiplying.
const READ_POOL_MAX: usize = 8;
const READ_POOL_MIN: usize = 2;

impl ReadPool {
    fn new(path: PathBuf) -> Self {
        let width = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(READ_POOL_MIN)
            .clamp(READ_POOL_MIN, READ_POOL_MAX);
        Self {
            path,
            slots: (0..width).map(|_| Mutex::new(None)).collect(),
            next: AtomicUsize::new(0),
            wait: TimingCounter::default(),
        }
    }

    /// An open connection, preferring one that already exists over paying for
    /// a new one, and blocking only when every slot is busy.
    fn acquire(&self) -> Option<MutexGuard<'_, Option<Connection>>> {
        let started = Instant::now();
        let width = self.slots.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % width;
        for offset in 0..width {
            if let Some(guard) = self.slots[(start + offset) % width].try_lock() {
                if guard.is_some() {
                    self.wait.observe(started.elapsed());
                    return Some(guard);
                }
            }
        }
        for offset in 0..width {
            if let Some(guard) = self.slots[(start + offset) % width].try_lock() {
                return self.populate(guard, started);
            }
        }
        self.populate(self.slots[start].lock(), started)
    }

    fn populate<'a>(
        &self,
        mut guard: MutexGuard<'a, Option<Connection>>,
        started: Instant,
    ) -> Option<MutexGuard<'a, Option<Connection>>> {
        if guard.is_none() {
            *guard = Some(connect_read_only(&self.path).ok()?);
        }
        self.wait.observe(started.elapsed());
        Some(guard)
    }
}

/// A connection a read may use. `Pooled` is always populated: `ReadPool`
/// opens the slot before handing the guard out, and falls back to `Writer`
/// rather than yielding an empty one.
enum CatalogRead<'a> {
    Pooled(MutexGuard<'a, Option<Connection>>),
    Writer(MutexGuard<'a, Connection>),
}

impl Deref for CatalogRead<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Pooled(guard) => guard
                .as_ref()
                .expect("a pooled read slot is populated before it is handed out"),
            Self::Writer(guard) => guard,
        }
    }
}

pub struct Meta {
    write: TimedMutex<Connection>,
    /// `None` for a read-only catalog: there is no writer to get out of the
    /// way of, and `open_read_only` may hold an `immutable=1` handle that a
    /// second plain read-only open could not reproduce.
    read: Option<ReadPool>,
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
/// Bytes in the first wal-index (`-shm`) region that SQLite maps.
///
/// `os_unix.c:unixShmMap` sparse-extends the wal-index by writing a single byte
/// to the last byte of every 4096-byte OS page in the region and then maps the
/// whole span. On a filesystem whose block size is smaller than the CPU page
/// size (mkfs.ext4 auto-selects 1024 or 2048 for images under 512 MB) those
/// one-byte writes allocate only the *final* block of each page, so a 32 KiB
/// wal-index is mapped while only 8 KiB of it is backed by disk blocks. When the
/// remaining 24 KiB cannot be allocated at fault time the kernel delivers
/// SIGBUS, which no Rust error path can observe: the process dies with wait
/// status 135, no exit code and empty stderr.
///
/// The value is SQLite's own region size, not a tuning knob: it is exactly the
/// span `unixShmMap` extends and maps for a fresh wal-index.
const WAL_INDEX_REGION_BYTES: u64 = 32768;

/// Free bytes available to this user on the filesystem holding `dir`.
///
/// `None` means the query itself failed; callers must then proceed rather than
/// invent a number, because refusing to open a repository on the strength of a
/// failed `statvfs` would be worse than the hazard it guards.
// The statvfs field widths are target-dependent (64-bit here, 32-bit elsewhere),
// so the widening casts below are only redundant on some of the targets we build.
#[allow(clippy::unnecessary_cast)]
fn available_bytes(dir: &Path) -> Option<u64> {
    let raw = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `raw` is a NUL-terminated path and `stat` is a live, correctly
    // sized allocation that libc only writes on success.
    let rc = unsafe { libc::statvfs(raw.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it initialized the whole struct.
    let stat = unsafe { stat.assume_init() };
    // Widening casts: both fields are unsigned, and are 32-bit on some targets.
    let blocks = stat.f_bavail as u64;
    let block_size = stat.f_frsize as u64;
    blocks.checked_mul(block_size)
}

/// Decide whether a wal-index may be created with `available` free bytes.
///
/// Separated from the syscall so the boundary is testable: at or above one
/// wal-index region the mapping can be fully backed, below it the process is in
/// the SIGBUS band and must fail as I/O instead of dying by signal.
fn wal_index_space_check(available: u64) -> Result<()> {
    if available >= WAL_INDEX_REGION_BYTES {
        return Ok(());
    }
    Err(Error::Io(format!(
        "refusing to open metadata: {available} bytes free on the filesystem \
         holding the repository, below the {WAL_INDEX_REGION_BYTES} bytes SQLite \
         maps for the wal-index; continuing risks SIGBUS instead of an error"
    )))
}

fn map_sql(e: rusqlite::Error) -> Error {
    use rusqlite::ffi::ErrorCode;
    if let rusqlite::Error::SqliteFailure(inner, ref message) = e {
        let text = message.clone().unwrap_or_else(|| inner.to_string());
        return match inner.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::Busy(text),
            ErrorCode::ConstraintViolation => Error::Invalid(text),
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                Error::Corrupt(format!("metadata catalog: {text}"))
            }
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

/// Rebuilds `observations` with the v2 shape, carrying every existing row
/// forward as the blob observation it was. Observations are session-scoped
/// mutable catalog rows and name no immutable bytes, so this rewrites metadata
/// only.
const MIGRATE_1_TO_2: &str = "\
CREATE TABLE observations_v2 (
  ns_id TEXT NOT NULL,
  mount TEXT NOT NULL,
  path  TEXT NOT NULL,
  kind  TEXT NOT NULL,
  oid   BLOB,
  PRIMARY KEY (ns_id, mount, path),
  CHECK((kind='absent' AND oid IS NULL)
        OR (kind IN ('blob','tree') AND length(oid)=32))
);
INSERT INTO observations_v2 (ns_id, mount, path, kind, oid)
  SELECT ns_id, mount, path, 'blob', oid FROM observations;
DROP TABLE observations;
ALTER TABLE observations_v2 RENAME TO observations;";

fn migrate(conn: &mut Connection, from: i64) -> Result<()> {
    if from > CURRENT_SCHEMA_VERSION {
        return Err(Error::Invalid(format!(
            "metadata schema version {from} is newer than supported {CURRENT_SCHEMA_VERSION}"
        )));
    }
    if from == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql)?;
    // Version 0 is TWO different states, and conflating them is a real bug:
    // a genuinely fresh catalog with no tables at all, and a *pre-versioning*
    // catalog that already carries v1-shaped tables but no ledger.
    // `CREATE TABLE IF NOT EXISTS` is a no-op against the second, so applying
    // SCHEMA alone leaves its `observations` at the v1 shape and the catalog
    // never reaches v2. verify_migrated_shape below catches that, but only
    // after the fact; a pre-versioning catalog needs the migration steps too.
    let pre_versioning = from == 0 && table_exists(&tx, "observations")?;
    if from == 0 {
        tx.execute_batch(SCHEMA).map_err(map_sql)?;
    }
    let first_step = if from == 0 { 1 } else { from };
    if from != 0 || pre_versioning {
        for step in first_step..CURRENT_SCHEMA_VERSION {
            match step {
                1 => tx.execute_batch(MIGRATE_1_TO_2).map_err(map_sql)?,
                _ => {
                    return Err(Error::Invalid(format!(
                        "unsupported metadata schema migration {step} -> {}",
                        step + 1
                    )))
                }
            }
        }
    }
    // `CREATE TABLE IF NOT EXISTS` is a no-op against a relation that already
    // exists with an older column list, so applying the current schema to a
    // pre-versioning catalog does not by itself prove the catalog reached it.
    // Recording a version the migration did not reach is worse than refusing:
    // the ledger would claim it forever, every later open would take the
    // "already current" path, and the first query would fail as a raw SQLite
    // error instead of a migration diagnostic. Checking inside the transaction
    // rolls the whole attempt back, so nothing is repaired and nothing is lost.
    verify_migrated_shape(&tx, from)?;
    let applied = now_ms() as i64;
    for version in (from + 1)..=CURRENT_SCHEMA_VERSION {
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, ?2)",
            params![version, applied],
        )
        .map_err(map_sql)?;
    }
    tx.commit().map_err(map_sql)
}

/// Confirm the catalog actually reached the shape `SCHEMA` describes.
/// Detection only: this never drops, rewrites, or repairs a relation, and it
/// never touches an immutable object (I17).
fn verify_migrated_shape(tx: &rusqlite::Transaction<'_>, from: i64) -> Result<()> {
    for (table, expected) in CATALOG_TABLES {
        let mut stmt = tx
            .prepare(&format!("PRAGMA table_info('{table}')"))
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(map_sql)?;
        let mut columns = Vec::new();
        for row in rows {
            columns.push(row.map_err(map_sql)?);
        }
        let actual: Vec<&str> = columns.iter().map(String::as_str).collect();
        if actual != **expected {
            return Err(Error::Invalid(format!(
                "metadata schema migration {from} -> {CURRENT_SCHEMA_VERSION} did not reach the current shape: table {table} has columns {actual:?}, expected {expected:?}"
            )));
        }
    }
    Ok(())
}

fn table_exists(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool> {
    let found: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .map_err(map_sql)?;
    Ok(found != 0)
}

/// True when `name` carries a terminal `abandon` reflog entry, i.e. the ref was
/// explicitly retired by `abandon_ref` and its row deliberately removed.
///
/// The reflog is the tombstone. `abandon_ref` is the only operation in this
/// store that deletes a `refs` row, and it always leaves an `abandon` entry
/// behind, so a name in that state is retired rather than missing. Two
/// consumers depend on that distinction: `audit_catalog` must not report the
/// surviving reflog rows as REFLOG_ORPHAN, and no creation path may resurrect
/// the name -- a resurrected ref would start a fresh `old_oid IS NULL` reflog
/// row on top of a chain that already has a terminal entry, which
/// `audit_catalog` correctly reports as REFLOG_CHAIN corruption.
fn ref_retired(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool> {
    let reason: Option<String> = tx
        .query_row(
            "SELECT reason FROM reflog WHERE name=?1 ORDER BY id DESC LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql)?;
    Ok(reason.as_deref() == Some(REFLOG_ABANDON))
}

/// Fail closed on any attempt to recreate a retired ref name.
fn deny_retired(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<()> {
    if ref_retired(tx, name)? {
        return Err(Error::Invalid(format!(
            "ref {name} was abandoned; the name is retired and cannot be recreated"
        )));
    }
    Ok(())
}

fn ref_exists(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool> {
    let found: i64 = tx
        .query_row("SELECT COUNT(*) FROM refs WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .map_err(map_sql)?;
    Ok(found != 0)
}

/// The ref-name grammar. Ref names are the one mutable surface a peer can
/// address by name, so the grammar is a trust boundary and is public in
/// order to be fuzzed directly rather than only through a whole repository.
pub fn validate_ref_name(name: &str) -> Result<()> {
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

/// The ref-name grammar plus the object type each ref namespace mandates.
pub fn validate_ref_kind(name: &str, kind: &str) -> Result<()> {
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

fn catalog_object_type(kind: &str) -> Option<ObjectType> {
    match kind {
        "blob" => Some(ObjectType::Blob),
        "tree" => Some(ObjectType::Tree),
        "commit" => Some(ObjectType::Commit),
        "conflict" => Some(ObjectType::Conflict),
        "snapshot" => Some(ObjectType::Snapshot),
        "contribution" => Some(ObjectType::Contribution),
        _ => None,
    }
}

fn catalog_ref_type(kind: &str) -> Option<ObjectType> {
    match kind {
        "tree" => Some(ObjectType::Tree),
        "commit" => Some(ObjectType::Commit),
        "conflict" => Some(ObjectType::Conflict),
        "snapshot" => Some(ObjectType::Snapshot),
        _ => None,
    }
}

impl CatalogAudit {
    fn finding(&mut self, code: &str, resource: impl Into<String>, detail: impl Into<String>) {
        self.findings.push(CatalogFinding {
            code: code.to_string(),
            resource: resource.into(),
            detail: detail.into(),
        });
    }

    fn oid(&mut self, bytes: Vec<u8>, resource: &str, field: &str) -> Option<ObjectId> {
        match oid_from_blob(bytes) {
            Ok(oid) => Some(oid),
            Err(error) => {
                self.finding(
                    "CATALOG_OID",
                    resource,
                    format!("{field} is not a 32-byte object ID: {error}"),
                );
                None
            }
        }
    }

    fn root(
        &mut self,
        oid: ObjectId,
        expected: CatalogObjectExpectation,
        resource: impl Into<String>,
    ) {
        self.roots.push(CatalogObjectRoot {
            oid,
            expected,
            resource: resource.into(),
        });
    }

    fn finish(&mut self) {
        self.findings.sort_by(|a, b| {
            (&a.code, &a.resource, &a.detail).cmp(&(&b.code, &b.resource, &b.detail))
        });
        self.findings.dedup();
        self.roots.sort_by(|a, b| a.resource.cmp(&b.resource));
        self.seals.sort_by(|a, b| a.tag.cmp(&b.tag));
        self.refs.sort_by(|a, b| a.name.cmp(&b.name));
        self.namespaces.sort_by(|a, b| a.id.cmp(&b.id));
        self.mounts
            .sort_by(|a, b| (&a.ns_id, &a.path).cmp(&(&b.ns_id, &b.path)));
    }
}

/// Prove that a table still has the V1 columns and SQLite storage classes
/// expected by ForgeFS before any typed row decoding happens. SQLite tables are
/// intentionally not STRICT in VERSION 1, so `PRAGMA integrity_check` alone
/// does not reject values such as `refs.protected='corrupt'`.
fn audit_table_shape(
    tx: &rusqlite::Transaction<'_>,
    audit: &mut CatalogAudit,
    table: &str,
    expected_columns: &[&str],
    valid_values: &str,
    schema_code: &str,
) -> bool {
    let pragma = format!("PRAGMA table_info('{table}')");
    let mut stmt = match tx.prepare(&pragma) {
        Ok(stmt) => stmt,
        Err(error) => {
            audit.finding(
                schema_code,
                format!("catalog:{table}"),
                format!("cannot inspect table shape: {error}"),
            );
            return false;
        }
    };
    let rows = match stmt.query_map([], |row| row.get::<_, String>(1)) {
        Ok(rows) => rows,
        Err(error) => {
            audit.finding(
                schema_code,
                format!("catalog:{table}"),
                format!("cannot read table shape: {error}"),
            );
            return false;
        }
    };
    let mut columns = Vec::new();
    for row in rows {
        match row {
            Ok(column) => columns.push(column),
            Err(error) => {
                audit.finding(
                    schema_code,
                    format!("catalog:{table}"),
                    format!("cannot decode table shape: {error}"),
                );
                return false;
            }
        }
    }
    let expected = expected_columns
        .iter()
        .map(|column| (*column).to_string())
        .collect::<Vec<_>>();
    if columns != expected {
        audit.finding(
            schema_code,
            format!("catalog:{table}"),
            format!("expected columns {expected:?}, found {columns:?}"),
        );
        return false;
    }
    drop(stmt);

    let sql = format!("SELECT rowid FROM \"{table}\" WHERE NOT ({valid_values}) ORDER BY rowid");
    let mut stmt = match tx.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            audit.finding(
                schema_code,
                format!("catalog:{table}"),
                format!("cannot validate table values: {error}"),
            );
            return false;
        }
    };
    let rows = match stmt.query_map([], |row| row.get::<_, i64>(0)) {
        Ok(rows) => rows,
        Err(error) => {
            audit.finding(
                schema_code,
                format!("catalog:{table}"),
                format!("cannot scan table values: {error}"),
            );
            return false;
        }
    };
    let value_code = if schema_code == "SCHEMA_LEDGER" {
        schema_code
    } else {
        "CATALOG_VALUE"
    };
    let mut clean = true;
    for row in rows {
        match row {
            Ok(rowid) => {
                clean = false;
                audit.finding(
                    value_code,
                    format!("catalog:{table}:row:{rowid}"),
                    "row violates ForgeFS storage-class or value constraints",
                );
            }
            Err(error) => {
                clean = false;
                audit.finding(
                    value_code,
                    format!("catalog:{table}"),
                    format!("cannot decode invalid row identity: {error}"),
                );
            }
        }
    }
    clean
}

impl Meta {
    pub fn stats(&self) -> MetaStats {
        let txn = self.stats.txn.snapshot();
        // Both mutexes, so the counter keeps meaning "every acquisition of a
        // process-local SQLite connection" now that reads have their own.
        let lock_wait = self.write.wait.snapshot();
        let read_wait = self
            .read
            .as_ref()
            .map(|pool| pool.wait.snapshot())
            .unwrap_or_default();
        MetaStats {
            txn_us: txn.total_us,
            txn_count: txn.count,
            lock_wait_us: lock_wait.total_us.saturating_add(read_wait.total_us),
            lock_acquires: lock_wait.count.saturating_add(read_wait.count),
            busy: self.stats.busy.load(Ordering::Relaxed),
            cas_updated: self.stats.cas_updated.load(Ordering::Relaxed),
            cas_forked: self.stats.cas_forked.load(Ordering::Relaxed),
            cas_denied: self.stats.cas_denied.load(Ordering::Relaxed),
            cas_noop: self.stats.cas_noop.load(Ordering::Relaxed),
        }
    }

    pub fn durability_policy(&self) -> &DurabilityPolicy {
        &self.durability
    }

    /// SQLite's `sqlite3_total_changes` for the write connection: every row
    /// inserted, updated, or deleted since it was opened.
    ///
    /// This is the honest instrument for metadata write amplification.
    /// `MetaStats::txn_count` counts only explicit `BEGIN IMMEDIATE` blocks and
    /// is blind to autocommit statements, so a read-heavy phase reports zero
    /// transactions while still dirtying one page per operation. Row mutations
    /// cannot be fooled that way.
    pub fn row_mutations(&self) -> u64 {
        self.write.lock().total_changes()
    }

    /// True when this catalog was opened read-only. Every write path is
    /// refused by SQLite itself (`query_only`), so this is a diagnostic, not
    /// the enforcement point.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Audit every relation that makes mutable catalog rows trustworthy.
    ///
    /// All queries run inside one deferred read transaction. In WAL mode this
    /// pins a single catalog snapshot without blocking writers, so a concurrent
    /// checkin cannot manufacture a false fsck finding between two queries.
    /// This method is detection-only and never repairs or normalizes rows.
    pub fn audit_catalog(&self) -> Result<CatalogAudit> {
        let mut conn = self.write.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(map_sql)?;
        let mut audit = CatalogAudit::default();

        {
            let mut stmt = tx.prepare("PRAGMA integrity_check").map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sql)?;
            for row in rows {
                let detail = row.map_err(map_sql)?;
                if !detail.eq_ignore_ascii_case("ok") {
                    audit.finding("CATALOG_INTEGRITY", "catalog:meta.sqlite", detail);
                }
            }
        }

        let migrations_clean = audit_table_shape(
            &tx,
            &mut audit,
            "schema_migrations",
            MIGRATION_COLUMNS,
            MIGRATION_VALUES,
            "SCHEMA_LEDGER",
        );
        let refs_clean = audit_table_shape(
            &tx,
            &mut audit,
            "refs",
            REFS_COLUMNS,
            REFS_VALUES,
            "CATALOG_SCHEMA",
        );
        let reflog_clean = audit_table_shape(
            &tx,
            &mut audit,
            "reflog",
            REFLOG_COLUMNS,
            REFLOG_VALUES,
            "CATALOG_SCHEMA",
        );
        let namespaces_clean = audit_table_shape(
            &tx,
            &mut audit,
            "namespaces",
            NAMESPACE_COLUMNS,
            NAMESPACE_VALUES,
            "CATALOG_SCHEMA",
        );
        let observations_clean = audit_table_shape(
            &tx,
            &mut audit,
            "observations",
            OBSERVATION_COLUMNS,
            OBSERVATION_VALUES,
            "CATALOG_SCHEMA",
        );
        let mounts_clean = audit_table_shape(
            &tx,
            &mut audit,
            "mounts",
            MOUNT_COLUMNS,
            MOUNT_VALUES,
            "CATALOG_SCHEMA",
        );
        let overlay_clean = audit_table_shape(
            &tx,
            &mut audit,
            "overlay",
            OVERLAY_COLUMNS,
            OVERLAY_VALUES,
            "CATALOG_SCHEMA",
        );
        let seals_clean = audit_table_shape(
            &tx,
            &mut audit,
            "seals",
            SEAL_COLUMNS,
            SEAL_VALUES,
            "CATALOG_SCHEMA",
        );
        let landmarks_clean = audit_table_shape(
            &tx,
            &mut audit,
            "landmarks",
            LANDMARK_COLUMNS,
            LANDMARK_VALUES,
            "CATALOG_SCHEMA",
        );
        let intros_clean = audit_table_shape(
            &tx,
            &mut audit,
            "object_intro",
            INTRO_COLUMNS,
            INTRO_VALUES,
            "CATALOG_SCHEMA",
        );
        audit_table_shape(
            &tx,
            &mut audit,
            "cap_root",
            CAP_ROOT_COLUMNS,
            CAP_ROOT_VALUES,
            "CATALOG_SCHEMA",
        );

        if migrations_clean {
            let mut stmt = tx
                .prepare("SELECT version FROM schema_migrations ORDER BY version")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(map_sql)?;
            let mut versions = Vec::new();
            for row in rows {
                versions.push(row.map_err(map_sql)?);
            }
            let expected_versions = (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>();
            if versions != expected_versions {
                audit.finding(
                    "SCHEMA_LEDGER",
                    "catalog:schema_migrations",
                    format!(
                        "expected contiguous supported versions {expected_versions:?}, found {versions:?}"
                    ),
                );
            }
        }

        let mut refs = BTreeMap::new();
        let mut refs_relations_clean = refs_clean;
        if refs_clean {
            let mut stmt = tx
                .prepare("SELECT name, oid, kind, protected, sealed FROM refs ORDER BY name")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (name, oid, kind, protected, sealed) = row.map_err(map_sql)?;
                let protected = protected != 0;
                let sealed = sealed != 0;
                let resource = format!("catalog:ref:{name}");
                if let Err(error) = validate_ref_kind(&name, &kind) {
                    audit.finding("REF_KIND", &resource, error.to_string());
                }
                let expected = match catalog_ref_type(&kind) {
                    Some(expected) => CatalogObjectExpectation::Exact(expected),
                    None => {
                        audit.finding("REF_KIND", &resource, format!("unknown ref kind {kind}"));
                        CatalogObjectExpectation::Any
                    }
                };
                if name.starts_with("tags/") && (!protected || !sealed || kind != "snapshot") {
                    audit.finding(
                        "TAG_FLAGS",
                        &resource,
                        "tag refs must be protected+sealed snapshots",
                    );
                }
                if let Some(oid) = audit.oid(oid, &resource, "oid") {
                    audit.root(oid, expected, format!("ref:{name}"));
                    audit.refs.push(RefRow {
                        name: name.clone(),
                        oid,
                        kind: kind.clone(),
                        protected,
                        sealed,
                    });
                    refs.insert(name, (oid, kind, protected, sealed));
                } else {
                    refs_relations_clean = false;
                }
            }
        }
        audit.refs_complete = refs_relations_clean;

        if refs_relations_clean && reflog_clean {
            let mut stmt = tx
                .prepare("SELECT id, name, old_oid, new_oid, reason FROM reflog ORDER BY name, id")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(map_sql)?;
            let mut reflog_rows = Vec::new();
            let mut reflog_oids_clean = true;
            for row in rows {
                let (id, name, old_oid, new_oid, reason) = row.map_err(map_sql)?;
                let resource = format!("catalog:reflog:{name}:{id}");
                let old_oid = match old_oid {
                    Some(oid) => match audit.oid(oid, &resource, "old_oid") {
                        Some(oid) => Some(oid),
                        None => {
                            reflog_oids_clean = false;
                            None
                        }
                    },
                    None => None,
                };
                let Some(new_oid) = audit.oid(new_oid, &resource, "new_oid") else {
                    reflog_oids_clean = false;
                    continue;
                };
                reflog_rows.push((id, name, old_oid, new_oid, reason));
            }

            if reflog_oids_clean {
                let mut reflog_names = BTreeSet::new();
                let mut terminal_reflog = BTreeMap::new();
                let mut previous_reflog = BTreeMap::new();
                for (id, name, old_oid, new_oid, reason) in reflog_rows {
                    reflog_names.insert(name.clone());
                    let resource = format!("catalog:reflog:{name}:{id}");
                    match previous_reflog.get(&name).copied() {
                        Some(previous) if old_oid != Some(previous) => audit.finding(
                            "REFLOG_CHAIN",
                            &resource,
                            format!(
                                "old_oid {:?} does not equal previous new_oid {previous}",
                                old_oid.map(|oid| oid.hex())
                            ),
                        ),
                        None if old_oid.is_some() && reason != "fork" => audit.finding(
                            "REFLOG_CHAIN",
                            &resource,
                            "first reflog row may have old_oid only when it creates a fork",
                        ),
                        _ => {}
                    }
                    previous_reflog.insert(name.clone(), new_oid);
                    terminal_reflog.insert(name, (new_oid, reason));
                }

                for (name, (ref_oid, _, _, _)) in &refs {
                    match terminal_reflog.get(name) {
                        None => audit.finding(
                            "REFLOG_MISSING",
                            format!("catalog:ref:{name}"),
                            "current ref has no reflog entry",
                        ),
                        Some((log_oid, _)) if log_oid != ref_oid => audit.finding(
                            "REFLOG_TERMINAL",
                            format!("catalog:ref:{name}"),
                            format!(
                                "current ref is {ref_oid}, terminal reflog new_oid is {log_oid}"
                            ),
                        ),
                        Some(_) => {}
                    }
                }
                for name in reflog_names {
                    // A chain that ends in `abandon` has no `refs` row on
                    // purpose: `abandon_ref` retired the name and left the
                    // reflog as the durable tombstone. That is the one
                    // legitimate way a reflog name outlives its ref.
                    let retired = terminal_reflog
                        .get(&name)
                        .is_some_and(|(_, reason)| reason == REFLOG_ABANDON);
                    if !refs.contains_key(&name) && !retired {
                        audit.finding(
                            "REFLOG_ORPHAN",
                            format!("catalog:reflog:{name}"),
                            "reflog name has no current ref",
                        );
                    }
                }
            }
        }

        let mut namespace_ids = BTreeSet::new();
        if namespaces_clean {
            let mut stmt = tx
                .prepare("SELECT id, pinned_oid, live_ref FROM namespaces ORDER BY id")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (id, pinned_oid, live_ref) = row.map_err(map_sql)?;
                let resource = format!("catalog:namespace:{id}");
                let pinned_oid = match pinned_oid {
                    Some(oid) => audit.oid(oid, &resource, "pinned_oid"),
                    None => {
                        audit.finding("NS_PIN", &resource, "namespace has no pinned commit");
                        None
                    }
                };
                if let Some(oid) = pinned_oid {
                    audit.root(
                        oid,
                        CatalogObjectExpectation::Exact(ObjectType::Commit),
                        format!("namespace:{id}:pin"),
                    );
                }
                if refs_relations_clean {
                    if let Some(live) = &live_ref {
                        match refs.get(live) {
                            Some((_, kind, _, _)) if kind == "commit" => {}
                            Some((_, kind, _, _)) => audit.finding(
                                "NS_LIVE_TYPE",
                                &resource,
                                format!("live ref {live} is {kind}, expected commit"),
                            ),
                            None => audit.finding(
                                "NS_LIVE_REF",
                                &resource,
                                format!("missing live ref {live}"),
                            ),
                        }
                    }
                }
                namespace_ids.insert(id.clone());
                audit.namespaces.push(CatalogNamespaceRow {
                    id,
                    pinned_oid,
                    live_ref,
                });
            }
        }

        let mut mount_keys = BTreeSet::new();
        if mounts_clean {
            let mut stmt = tx
                .prepare("SELECT ns_id, path, spec, mode FROM mounts ORDER BY ns_id, path")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (ns_id, path, spec, mode) = row.map_err(map_sql)?;
                if namespaces_clean && !namespace_ids.contains(&ns_id) {
                    audit.finding(
                        "MOUNT_NAMESPACE",
                        format!("catalog:mount:{ns_id}:{path}"),
                        "mount refers to a missing namespace",
                    );
                }
                mount_keys.insert((ns_id.clone(), path.clone()));
                audit.mounts.push(CatalogMountRow {
                    ns_id,
                    path,
                    spec,
                    mode,
                });
            }
        }

        if observations_clean {
            let mut stmt = tx
                .prepare(
                    "SELECT ns_id, mount, path, kind, oid FROM observations \
                     ORDER BY ns_id, mount, path",
                )
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (ns_id, mount, path, kind, oid) = row.map_err(map_sql)?;
                let resource = format!("catalog:observation:{ns_id}:{mount}:{path}");
                if namespaces_clean && !namespace_ids.contains(&ns_id) {
                    audit.finding(
                        "OBSERVATION_NAMESPACE",
                        &resource,
                        "observation refers to a missing namespace",
                    );
                } else if mounts_clean && !mount_keys.contains(&(ns_id.clone(), mount.clone())) {
                    audit.finding(
                        "OBSERVATION_MOUNT",
                        &resource,
                        "observation refers to a missing mount",
                    );
                }
                // An absent observation names no bytes, so it contributes no
                // reachability root. A directory observation names a tree.
                let expected = match kind.as_str() {
                    "blob" => Some(ObjectType::Blob),
                    "tree" => Some(ObjectType::Tree),
                    _ => None,
                };
                if let (Some(expected), Some(oid)) = (expected, oid) {
                    if let Some(oid) = audit.oid(oid, &resource, "oid") {
                        audit.root(
                            oid,
                            CatalogObjectExpectation::Exact(expected),
                            format!("namespace:{ns_id}:observation:{mount}:{path}"),
                        );
                    }
                }
            }
        }

        if overlay_clean {
            let mut stmt = tx
                .prepare(
                    "SELECT ns_id, mount, path, blob_oid FROM overlay ORDER BY ns_id, mount, path",
                )
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (ns_id, mount, path, blob_oid) = row.map_err(map_sql)?;
                let resource = format!("catalog:overlay:{ns_id}:{mount}:{path}");
                if namespaces_clean && !namespace_ids.contains(&ns_id) {
                    audit.finding(
                        "OVERLAY_NAMESPACE",
                        &resource,
                        "overlay refers to a missing namespace",
                    );
                } else if mounts_clean && !mount_keys.contains(&(ns_id.clone(), mount.clone())) {
                    audit.finding(
                        "OVERLAY_MOUNT",
                        &resource,
                        "overlay refers to a missing mount",
                    );
                }
                if let Some(oid) = blob_oid.and_then(|oid| audit.oid(oid, &resource, "blob_oid")) {
                    audit.root(
                        oid,
                        CatalogObjectExpectation::Exact(ObjectType::Blob),
                        format!("namespace:{ns_id}:mount:{mount}:overlay:{path}"),
                    );
                }
            }
        }

        let mut seal_tags = BTreeSet::new();
        if seals_clean {
            let mut stmt = tx
                .prepare("SELECT tag, snap_oid, commit_oid, tree_oid FROM seals ORDER BY tag")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (tag, snap_oid, commit_oid, tree_oid) = row.map_err(map_sql)?;
                seal_tags.insert(tag.clone());
                let resource = format!("catalog:seal:{tag}");
                let snap_oid = audit.oid(snap_oid, &resource, "snap_oid");
                let commit_oid = audit.oid(commit_oid, &resource, "commit_oid");
                let tree_oid = audit.oid(tree_oid, &resource, "tree_oid");
                let (Some(snap_oid), Some(commit_oid), Some(tree_oid)) =
                    (snap_oid, commit_oid, tree_oid)
                else {
                    continue;
                };
                audit.root(
                    snap_oid,
                    CatalogObjectExpectation::Exact(ObjectType::Snapshot),
                    format!("{resource}:snapshot"),
                );
                audit.root(
                    commit_oid,
                    CatalogObjectExpectation::Exact(ObjectType::Commit),
                    format!("{resource}:commit"),
                );
                audit.root(
                    tree_oid,
                    CatalogObjectExpectation::Exact(ObjectType::Tree),
                    format!("{resource}:tree"),
                );
                audit.seals.push(SealRow {
                    tag: tag.clone(),
                    snap_oid,
                    commit_oid,
                    tree_oid,
                });

                if refs_relations_clean {
                    let tag_ref = format!("tags/{tag}");
                    match refs.get(&tag_ref) {
                        None => audit.finding(
                            "SEAL_TAG_REF",
                            &resource,
                            format!("missing sealed ref {tag_ref}"),
                        ),
                        Some((oid, kind, protected, sealed))
                            if *oid != snap_oid
                                || kind != "snapshot"
                                || !*protected
                                || !*sealed =>
                        {
                            audit.finding(
                                "SEAL_TAG_REF",
                                &resource,
                                format!("{tag_ref} must be protected+sealed snapshot {snap_oid}"),
                            )
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        if refs_relations_clean && seals_clean {
            for name in refs.keys().filter(|name| name.starts_with("tags/")) {
                let tag = name.trim_start_matches("tags/");
                if !seal_tags.contains(tag) {
                    audit.finding(
                        "SEAL_ROW",
                        format!("catalog:ref:{name}"),
                        "sealed tag ref has no seals row",
                    );
                }
            }
        }

        let mut landmarks = BTreeMap::new();
        if landmarks_clean {
            let mut stmt = tx
                .prepare("SELECT oid, lower(hex(oid)), kind FROM landmarks ORDER BY oid")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (oid, hex, kind) = row.map_err(map_sql)?;
                let resource = format!("catalog:landmark:{hex}");
                let Some(oid) = audit.oid(oid, &resource, "oid") else {
                    continue;
                };
                let expected = match catalog_object_type(&kind) {
                    Some(expected) => CatalogObjectExpectation::Exact(expected),
                    None => {
                        audit.finding(
                            "LANDMARK_KIND",
                            &resource,
                            format!("unknown landmark kind {kind}"),
                        );
                        CatalogObjectExpectation::Any
                    }
                };
                audit.root(oid, expected, &resource);
                landmarks.insert(oid, kind);
            }
        }

        if seals_clean && landmarks_clean {
            for seal in audit.seals.clone() {
                let mut missing = Vec::new();
                if landmarks.get(&seal.snap_oid).map(String::as_str) != Some("snapshot") {
                    missing.push(format!("snapshot landmark {}", seal.snap_oid));
                }
                if landmarks.get(&seal.commit_oid).map(String::as_str) != Some("commit") {
                    missing.push(format!("commit landmark {}", seal.commit_oid));
                }
                if !missing.is_empty() {
                    audit.finding(
                        "SEAL_LANDMARK",
                        format!("catalog:seal:{}", seal.tag),
                        format!("missing or mistyped {}", missing.join(" and ")),
                    );
                }
            }
        }

        if intros_clean {
            let mut stmt = tx
                .prepare("SELECT oid, lower(hex(oid)), commit_oid FROM object_intro ORDER BY oid")
                .map_err(map_sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(map_sql)?;
            for row in rows {
                let (oid, hex, commit_oid) = row.map_err(map_sql)?;
                let resource = format!("catalog:object_intro:{hex}");
                if let Some(oid) = audit.oid(oid, &resource, "oid") {
                    audit.root(oid, CatalogObjectExpectation::TreeEntry, &resource);
                }
                if let Some(commit_oid) = audit.oid(commit_oid, &resource, "commit_oid") {
                    audit.root(
                        commit_oid,
                        CatalogObjectExpectation::Exact(ObjectType::Commit),
                        format!("{resource}:commit"),
                    );
                }
            }
        }

        tx.commit().map_err(map_sql)?;
        audit.finish();
        Ok(audit)
    }

    /// Complete a truncating WAL checkpoint through the same connection whose
    /// FULL durability policy was verified during `open`. SQLite reports a
    /// busy checkpoint in the result row rather than as an execution error, so
    /// treat partial completion as an explicit failure.
    pub fn checkpoint_truncate(&self) -> Result<CheckpointResult> {
        // SQLite owns the VFS barriers inside this call. Bracket the operation
        // so the state machine covers both "never started" and "completed but
        // not acknowledged" failures without installing a production VFS.
        crate::inject_barrier_failure(crate::DurabilityBarrier::MetadataCheckpointBefore)?;
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
        crate::inject_barrier_failure(crate::DurabilityBarrier::MetadataCheckpointAfter)?;
        Ok(CheckpointResult {
            log_frames,
            checkpointed_frames,
        })
    }

    /// The connection a SELECT-only query must use.
    ///
    /// Only for statements that cannot write. Anything inside an explicit
    /// transaction, and any read whose result must reflect a write this same
    /// logical operation has not committed yet, stays on the write connection:
    /// a WAL reader sees the last committed state, which is exactly right for
    /// a read that follows a committed write and exactly wrong for one that
    /// does not.
    fn read_conn(&self) -> CatalogRead<'_> {
        if let Some(pool) = &self.read {
            if let Some(guard) = pool.acquire() {
                return CatalogRead::Pooled(guard);
            }
        }
        CatalogRead::Writer(self.write.lock())
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

    fn commit_ref_tx(&self, tx: rusqlite::Transaction<'_>) -> Result<()> {
        tx.commit().map_err(|error| self.map_sql_counted(error))?;
        crate::inject_barrier_failure(crate::DurabilityBarrier::MetadataRefCommitAfter)
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
        // Checked before SQLite can create the wal-index: inside the band the
        // mapping is made but cannot be backed, and the process dies by SIGBUS
        // with no exit code at all. See wal_index_space_check.
        //
        // Writable opens only. A read-only open never creates the wal-index, so
        // it has no sparse mapping to fault on, and refusing it here would make
        // `fsck`/`verify` unavailable on a nearly-full filesystem -- precisely
        // when an operator needs a diagnostic.
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        if let Some(available) = available_bytes(dir) {
            wal_index_space_check(available)?;
        }
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
            // Safe to build now and not before: the writable open has created
            // the database, switched it to WAL and mapped the wal-index, so a
            // later read-only member has both a file and a `-shm` to attach.
            read: Some(ReadPool::new(path.to_path_buf())),
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
        Self::open_read_only_mode(path, true)
    }

    /// Fsck must be able to inspect and report a damaged migration ledger
    /// instead of refusing before the audit starts. This keeps every physical
    /// read-only guarantee of `open_read_only`, but defers ledger compatibility
    /// to `audit_catalog`. No other command may use this path.
    pub fn open_read_only_for_fsck(path: &Path) -> Result<Self> {
        Self::open_read_only_mode(path, false)
    }

    fn open_read_only_mode(path: &Path, enforce_current_ledger: bool) -> Result<Self> {
        // The wal-index hazard is not about writing, it is about whether SQLite
        // will MAP one. A read-only open of a WAL catalog on writable media
        // still creates and maps .forge/meta.sqlite-shm, so it can fault into an
        // unbacked page and die by SIGBUS exactly like a writable open -- the
        // probe caught `fsck` doing so at 9216 and 8192 bytes free.
        //
        // Only the `immutable=1` path below is genuinely exempt, because it
        // skips the -shm entirely. That is also the case that matters most:
        // read-only MEDIA keeps working, while a nearly-full writable
        // filesystem gets a clear exit 5 instead of a signal death.
        if !media_is_read_only(path) {
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            if let Some(available) = available_bytes(dir) {
                wal_index_space_check(available)?;
            }
        }
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

        if enforce_current_ledger {
            let version = Self::compatible_schema_version(&conn)?;
            if version < CURRENT_SCHEMA_VERSION {
                // Migration is a write. Say so, instead of failing later inside an
                // arbitrary query against a column this schema does not have.
                return Err(Error::Invalid(format!(
                    "metadata schema version {version} needs migration to {CURRENT_SCHEMA_VERSION}, which a read-only open cannot perform"
                )));
            }
        }
        let durability = Self::observe_durability(&conn, true)?;
        Ok(Self {
            write: TimedMutex::new(conn),
            read: None,
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
        let conn = self.read_conn();
        conn.query_row("SELECT seal_pub FROM cap_root WHERE id=1", [], |r| r.get(0))
            .map_err(|_| Error::Corrupt("missing cap_root".into()))
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        let conn = self.read_conn();
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
        deny_retired(&tx, name)?;
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
        deny_retired(&tx, name)?;
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
            deny_retired(&tx, name)?;
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
            self.commit_ref_tx(tx)?;
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
            self.commit_ref_tx(tx)?;
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
        self.commit_ref_tx(tx)?;
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
            deny_retired(&tx, name)?;
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
            self.commit_ref_tx(tx)?;
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
            self.commit_ref_tx(tx)?;
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
        self.commit_ref_tx(tx)?;
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
        self.commit_ref_tx(tx)?;
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
        // The only path that completes a checkin without publishing a commit,
        // so it is the only place a no-op outcome can be counted. It is
        // deliberately not folded into `cas_updated`/`cas_forked`: no ref CAS
        // was attempted here.
        self.stats.cas_noop.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn list_refs(&self) -> Result<Vec<RefRow>> {
        let conn = self.read_conn();
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
        let conn = self.read_conn();
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
        let conn = self.read_conn();
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

    /// Record what a read saw, so I9 can validate it at checkin.
    ///
    /// I9 constrains what the `observations` table *holds* when checkin reads
    /// it, not how many times it was written on the way there. Re-reading a
    /// path whose row already records the same `(kind, oid)` would have
    /// `INSERT OR REPLACE` delete and re-insert a byte-identical row: one
    /// dirtied page, one WAL commit and, under `synchronous=FULL`, one fsync,
    /// to arrive at the state the table is already in. Issue #308 measured
    /// exactly one row mutation per read op in every workload shape while the
    /// row count stayed pinned at the distinct-path count -- 3650 write
    /// transactions to hold 146 rows in the grep-heavy shape.
    ///
    /// So look first, on a read connection where the check costs no
    /// write-mutex time, and write only when the row would actually change.
    /// The table ends in the same state either way.
    ///
    /// Concurrency: the look and the write are not one atomic step, so two
    /// threads observing the same path with different OIDs still race. That
    /// race is unchanged -- `INSERT OR REPLACE` was already last-writer-wins
    /// for them -- and it cannot occur at all for the equal-value case this
    /// skips, where every racer is recording the same bytes.
    ///
    /// A read-only catalog is excluded: it must keep refusing the write rather
    /// than report success on the strength of a row it did not record.
    pub fn observe(&self, ns_id: &str, mount: &str, path: &str, seen: Observed) -> Result<()> {
        let oid = seen.oid().map(|id| id.as_bytes().to_vec());
        if !self.read_only && self.observation_is_current(ns_id, mount, path, seen.kind(), &oid)? {
            return Ok(());
        }
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO observations (ns_id, mount, path, kind, oid) \
             VALUES (?1,?2,?3,?4,?5)",
            params![ns_id, mount, path, seen.kind(), oid],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    /// True when `observations` already records exactly this outcome for the
    /// path, so writing it again would change no row.
    fn observation_is_current(
        &self,
        ns_id: &str,
        mount: &str,
        path: &str,
        kind: &str,
        oid: &Option<Vec<u8>>,
    ) -> Result<bool> {
        let conn = self.read_conn();
        let stored = conn
            .query_row(
                "SELECT kind, oid FROM observations WHERE ns_id=?1 AND mount=?2 AND path=?3",
                params![ns_id, mount, path],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()
            .map_err(map_sql)?;
        Ok(match stored {
            Some((stored_kind, stored_oid)) => stored_kind == kind && stored_oid == *oid,
            None => false,
        })
    }

    pub fn observations(&self, ns_id: &str) -> Result<Vec<ObservationRow>> {
        let conn = self.read_conn();
        let mut stmt = conn
            .prepare("SELECT mount, path, kind, oid FROM observations WHERE ns_id=?1")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([ns_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (mount, path, kind, oid) = row.map_err(map_sql)?;
            // Fail closed on an unknown discriminant rather than silently
            // downgrading the observation to "nothing was read".
            let seen = match (kind.as_str(), oid) {
                ("blob", Some(oid)) => Observed::Blob(oid_from_blob(oid)?),
                ("tree", Some(oid)) => Observed::Tree(oid_from_blob(oid)?),
                ("absent", None) => Observed::Absent,
                (other, _) => {
                    return Err(Error::Invalid(format!(
                        "observation {mount}:{path} has unusable kind {other}"
                    )))
                }
            };
            out.push(ObservationRow { mount, path, seen });
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
        let conn = self.read_conn();
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
        // I6/I10: an accepted overlay must describe one representable tree.
        // Make the prefix check and mutation one IMMEDIATE transaction so two
        // processes cannot concurrently stage an ancestor and descendant.
        // Exact-path replacement remains legal and keeps ordinary overwrite
        // semantics for a path already staged by this namespace.
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let conflict = tx
            .query_row(
                "SELECT path FROM overlay
                 WHERE ns_id=?1 AND mount=?2 AND path<>?3 AND (
                   (length(path) < length(?3)
                    AND substr(?3,1,length(path))=path
                    AND substr(?3,length(path)+1,1)='/')
                   OR
                   (length(path) > length(?3)
                    AND substr(path,1,length(?3))=?3
                    AND substr(path,length(?3)+1,1)='/')
                 )
                 ORDER BY path
                 LIMIT 1",
                params![ns_id, mount, path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sql)?;
        if let Some(existing) = conflict {
            return Err(Error::Invalid(format!(
                "overlay path conflict: {path} and {existing} cannot coexist"
            )));
        }
        let oid = blob_oid.map(|o| o.0.to_vec());
        tx.execute(
            "INSERT OR REPLACE INTO overlay (ns_id, mount, path, blob_oid, exec) VALUES (?1,?2,?3,?4,?5)",
            params![ns_id, mount, path, oid, exec as i64],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        txn_timer.finish();
        Ok(())
    }

    pub fn overlay_list(&self, ns_id: &str, mount: &str) -> Result<Vec<OverlayRow>> {
        let conn = self.read_conn();
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
        let conn = self.read_conn();
        conn.query_row(
            "SELECT agent_id FROM object_intro WHERE oid=?1",
            params![oid.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql)
    }

    /// Explicitly retire a fork ref, removing it from the GC root set.
    ///
    /// This is the only operation in ForgeFS that deletes a `refs` row, and it
    /// is a deliberate act rather than a failure path, so I18 is untouched: a
    /// refused checkin still forks and still keeps the work. What it adds is
    /// the other half of I18 -- every losing CAS mints a fork ref that pins an
    /// object closure forever, and until now nothing could retire one.
    ///
    /// The row is replaced by a terminal `abandon` reflog entry recording the
    /// retired OID, the agent and the time, so the work stays addressable by
    /// OID and auditable by name. `ref_retired` makes that entry load-bearing
    /// for `audit_catalog` and for every ref-creation path.
    ///
    /// Refused, inside the same immediate transaction that would delete the
    /// row, when the ref is protected, sealed, still the live head of a
    /// namespace, or still named by a mount. Those are exactly the states in
    /// which a concurrent session can still resolve the name, and a dangling
    /// mount or live_ref is the corruption `fsck` reports as exit 2.
    pub fn abandon_ref(&self, name: &str, agent_id: &str) -> Result<RetiredRef> {
        validate_ref_name(name)?;
        if !name.starts_with(ABANDONABLE_PREFIX) {
            return Err(Error::Invalid(format!(
                "only {ABANDONABLE_PREFIX}* refs may be abandoned, not {name}"
            )));
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
        let Some((oid, kind, protected, sealed)) = row else {
            if ref_retired(&tx, name)? {
                return Err(Error::Invalid(format!("ref {name} is already abandoned")));
            }
            return Err(Error::NotFound(format!("ref {name}")));
        };
        if sealed != 0 {
            return Err(Error::Sealed(name.to_string()));
        }
        if protected != 0 {
            return Err(Error::Denied(format!("ref {name} is protected")));
        }
        let oid = oid_from_blob(oid)?;
        let live: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM namespaces WHERE live_ref=?1",
                [name],
                |r| r.get(0),
            )
            .map_err(|error| self.map_sql_counted(error))?;
        if live != 0 {
            return Err(Error::Invalid(format!(
                "ref {name} is the live head of an open session"
            )));
        }
        let mounted: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM mounts WHERE spec=?1 OR spec=?2",
                params![name, format!("ref:{name}")],
                |r| r.get(0),
            )
            .map_err(|error| self.map_sql_counted(error))?;
        if mounted != 0 {
            return Err(Error::Invalid(format!(
                "ref {name} is still mounted by an open session"
            )));
        }
        let ts = now_ms() as i64;
        // I6: the ref change and its reflog entry commit together. The entry
        // chains old_oid -> new_oid on the same OID, because the ref did not
        // move -- it stopped existing -- so REFLOG_CHAIN stays satisfied.
        tx.execute(
            "INSERT INTO reflog (name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                name,
                oid.as_bytes().as_slice(),
                oid.as_bytes().as_slice(),
                agent_id,
                REFLOG_ABANDON,
                ts
            ],
        )
        .map_err(|error| self.map_sql_counted(error))?;
        let removed = tx
            .execute("DELETE FROM refs WHERE name=?1", [name])
            .map_err(|error| self.map_sql_counted(error))?;
        if removed != 1 {
            return Err(Error::Internal(format!(
                "abandon removed {removed} rows for ref {name}"
            )));
        }
        self.commit_ref_tx(tx)?;
        txn_timer.finish();
        Ok(RetiredRef {
            name: name.to_string(),
            oid,
            kind,
            agent_id: agent_id.to_string(),
            ts_ms: ts,
        })
    }

    /// Explicitly retire a session, removing its pin, mounts, overlay and
    /// observations from the GC root set.
    ///
    /// Uncommitted overlay entries ARE staged work, so a session holding any is
    /// refused unless the caller passes `discard_staged`. That keeps the I18
    /// spirit -- work is never destroyed by a path the caller did not choose --
    /// while giving a stranded session the escape hatch it never had.
    ///
    /// The session's live head ref is deliberately left alone: it is published
    /// history under `heads/`, not the fork churn this verb exists to bound.
    pub fn abandon_session(&self, ns_id: &str, discard_staged: bool) -> Result<AbandonedSession> {
        let mut conn = self.write.lock();
        let txn_timer = self.txn_timer();
        let tx = self.begin_tx(&mut conn)?;
        let exists: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM namespaces WHERE id=?1",
                [ns_id],
                |r| r.get(0),
            )
            .map_err(|error| self.map_sql_counted(error))?;
        if exists == 0 {
            return Err(Error::NotFound(format!("namespace {ns_id}")));
        }
        let staged: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM overlay WHERE ns_id=?1",
                [ns_id],
                |r| r.get(0),
            )
            .map_err(|error| self.map_sql_counted(error))?;
        if staged != 0 && !discard_staged {
            return Err(Error::Invalid(format!(
                "session {ns_id} has {staged} staged overlay entries; check in first or abandon with the explicit discard flag"
            )));
        }
        let mounts = tx
            .execute("DELETE FROM mounts WHERE ns_id=?1", [ns_id])
            .map_err(|error| self.map_sql_counted(error))?;
        let observations = tx
            .execute("DELETE FROM observations WHERE ns_id=?1", [ns_id])
            .map_err(|error| self.map_sql_counted(error))?;
        tx.execute("DELETE FROM overlay WHERE ns_id=?1", [ns_id])
            .map_err(|error| self.map_sql_counted(error))?;
        tx.execute("DELETE FROM namespaces WHERE id=?1", [ns_id])
            .map_err(|error| self.map_sql_counted(error))?;
        self.commit_ref_tx(tx)?;
        txn_timer.finish();
        Ok(AbandonedSession {
            ns_id: ns_id.to_string(),
            discarded_overlay: staged as usize,
            removed_mounts: mounts,
            removed_observations: observations,
        })
    }

    /// Every landmark OID with the kind it was recorded as. Landmarks are GC
    /// roots by construction (#249), so this is a root reader, not a report.
    pub fn list_landmarks(&self) -> Result<Vec<(ObjectId, String)>> {
        let conn = self.read_conn();
        let mut stmt = conn
            .prepare("SELECT oid, kind FROM landmarks ORDER BY oid")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (oid, kind) = row.map_err(map_sql)?;
            out.push((oid_from_blob(oid)?, kind));
        }
        Ok(out)
    }

    /// Every sealed tag with the three OIDs its manifest binds.
    pub fn list_seals(&self) -> Result<Vec<(String, ObjectId, ObjectId, ObjectId)>> {
        let conn = self.read_conn();
        let mut stmt = conn
            .prepare("SELECT tag, snap_oid, commit_oid, tree_oid FROM seals ORDER BY tag")
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (tag, snap, commit, tree) = row.map_err(map_sql)?;
            out.push((
                tag,
                oid_from_blob(snap)?,
                oid_from_blob(commit)?,
                oid_from_blob(tree)?,
            ));
        }
        Ok(out)
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
        let conn = self.read_conn();
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
        let conn = self.read_conn();
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

    /// Path of the wal-index sidecar SQLite derives from a database path.
    fn wal_index_path(db: &Path) -> std::path::PathBuf {
        let mut name = db.as_os_str().to_os_string();
        name.push("-shm");
        std::path::PathBuf::from(name)
    }

    /// The band this guards is not "out of space": at 0 bytes free SQLite
    /// already fails cleanly. It is the range where the wal-index mapping is
    /// created but cannot be fully backed.
    #[test]
    fn wal_index_space_check_rejects_only_below_one_region() {
        assert!(wal_index_space_check(WAL_INDEX_REGION_BYTES).is_ok());
        assert!(wal_index_space_check(WAL_INDEX_REGION_BYTES * 4096).is_ok());
        for available in [0u64, 3072, 7168, 9216, 15360, WAL_INDEX_REGION_BYTES - 1] {
            let error = wal_index_space_check(available)
                .expect_err("below one wal-index region must fail as I/O, never by signal");
            assert!(
                matches!(error, Error::Io(_)),
                "the near-full filesystem band must map to exit 5, got {error:?}"
            );
            assert!(
                error.to_string().contains("wal-index"),
                "diagnostic must name the wal-index: {error}"
            );
        }
    }

    #[test]
    fn wal_index_path_is_the_sqlite_shm_sidecar() {
        assert_eq!(
            wal_index_path(Path::new("/x/.forge/meta.sqlite")),
            std::path::PathBuf::from("/x/.forge/meta.sqlite-shm")
        );
    }

    /// Characterisation of the SIGBUS precondition.
    ///
    /// After `Meta::open`, no page of the first wal-index region may be a hole,
    /// because SQLite has already mapped the whole region and a fault into a
    /// hole on a full filesystem is delivered as SIGBUS, not as an error.
    ///
    /// On a filesystem whose block size equals the CPU page size, SQLite's own
    /// one-byte-per-page extension already allocates everything and this holds
    /// without our preallocation. Point `TMPDIR` at an ext4 image small enough
    /// that mkfs picked a 1024- or 2048-byte block (see
    /// `scripts/enospc-sigbus-probe.sh`) and it fails without `back_wal_index`.
    #[test]
    #[cfg(target_os = "linux")]
    fn open_leaves_no_hole_in_the_mapped_wal_index() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempdir().unwrap();
        let db = dir.path().join("meta.sqlite");
        let meta = Meta::open(&db).unwrap();
        let shm = std::fs::metadata(wal_index_path(&db))
            .expect("WAL mode must have created the wal-index");
        let mapped = shm.len().min(WAL_INDEX_REGION_BYTES);
        let backed = shm.blocks() * 512;
        assert!(
            backed >= mapped,
            "wal-index maps {mapped} bytes but only {backed} are backed by blocks; \
             a page fault into the hole on a near-full filesystem is SIGBUS, \
             which no exit code can report"
        );
        drop(meta);
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
