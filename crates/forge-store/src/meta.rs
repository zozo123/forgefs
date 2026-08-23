use forge_core::now_ms;
use forge_types::{CasResult, Error, ObjectId, RefRow, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
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
    pub txn_us: u64,
    pub busy: u64,
    pub cas_updated: u64,
    pub cas_forked: u64,
    pub cas_denied: u64,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Debug, Default)]
struct MetaCounters {
    txn_us: AtomicU64,
    busy: AtomicU64,
    cas_updated: AtomicU64,
    cas_forked: AtomicU64,
    cas_denied: AtomicU64,
}

struct TxnTimer<'a> {
    started: Instant,
    total_us: &'a AtomicU64,
}

impl Drop for TxnTimer<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_micros();
        self.total_us.fetch_add(
            u64::try_from(elapsed).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

pub struct Meta {
    write: Mutex<Connection>,
    stats: MetaCounters,
    durability: DurabilityPolicy,
}

fn oid_from_blob(v: Vec<u8>) -> Result<ObjectId> {
    if v.len() != 32 {
        return Err(Error::Corrupt("oid blob length".into()));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(ObjectId(a))
}

fn map_sql(e: rusqlite::Error) -> Error {
    let s = e.to_string();
    if s.contains("database is locked") || s.contains("busy") {
        Error::Busy(s)
    } else {
        Error::Sqlite(s)
    }
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
        MetaStats {
            txn_us: self.stats.txn_us.load(Ordering::Relaxed),
            busy: self.stats.busy.load(Ordering::Relaxed),
            cas_updated: self.stats.cas_updated.load(Ordering::Relaxed),
            cas_forked: self.stats.cas_forked.load(Ordering::Relaxed),
            cas_denied: self.stats.cas_denied.load(Ordering::Relaxed),
        }
    }

    pub fn durability_policy(&self) -> &DurabilityPolicy {
        &self.durability
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
            total_us: &self.stats.txn_us,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(map_sql)?;

        // Connection-scoped only: preserve the normal five-second contention
        // policy without changing an incompatible database on disk.
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(map_sql)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;

        // Compatibility checks are read-only. Do not mutate a repository that
        // this binary has already determined it cannot understand.
        let version = schema_version(&conn)?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "metadata schema version {version} is newer than supported {CURRENT_SCHEMA_VERSION}"
            )));
        }

        // Once compatible, establish the persistent durability contract before
        // any schema migration or metadata write.
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
        let fullfsync = {
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
                Some(true)
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        };

        migrate(&mut conn, version)?;
        conn.execute(
            "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
            [],
        )
        .map_err(map_sql)?;
        Ok(Self {
            write: Mutex::new(conn),
            stats: MetaCounters::default(),
            durability: DurabilityPolicy {
                journal_mode: journal_mode.to_ascii_lowercase(),
                synchronous,
                fullfsync,
            },
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
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
        tx.commit().map_err(map_sql)?;
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
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
        let tx = self.begin_tx(&mut conn)?;
        let _txn_timer = self.txn_timer();
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
        let tx = self.begin_tx(&mut conn)?;
        let _txn_timer = self.txn_timer();
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
        let tx = self.begin_tx(&mut conn)?;
        let _txn_timer = self.txn_timer();
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let ts = now_ms() as i64;
        Self::insert_intros_tx(&tx, oids, commit, agent_id, ts)?;
        tx.commit().map_err(map_sql)?;
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let ts = now_ms() as i64;
        let tag_ref = format!("tags/{tag}");
        validate_ref_kind(&tag_ref, "snapshot")?;
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
