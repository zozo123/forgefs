from pathlib import Path
import re

p = Path("crates/forge-store/src/meta.rs")
s = p.read_text()

marker = '#[derive(Clone, Debug)]\npub struct MountRow {'
if marker not in s:
    raise SystemExit("schema constant insertion anchor missing")
s = s.replace(
    marker,
    'pub const SCHEMA_VERSION: i64 = 1;\n\n' + marker,
    1,
)

impl_marker = 'impl Meta {\n'
if s.count(impl_marker) != 1:
    raise SystemExit("Meta impl anchor not unique")
helpers = r'''fn metadata_version(conn: &Connection) -> Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sql)
}

fn verify_schema_v1(conn: &Connection) -> Result<()> {
    const CHECKS: &[&str] = &[
        "SELECT name, oid, kind, protected, sealed, updated_ms FROM refs LIMIT 0",
        "SELECT id, name, old_oid, new_oid, agent_id, reason, ts_ms FROM reflog LIMIT 0",
        "SELECT id, agent_id, created_ms, pinned_oid, live_ref FROM namespaces LIMIT 0",
        "SELECT ns_id, mount, path, oid FROM observations LIMIT 0",
        "SELECT ns_id, path, spec, mode FROM mounts LIMIT 0",
        "SELECT ns_id, mount, path, blob_oid, exec FROM overlay LIMIT 0",
        "SELECT tag, snap_oid, commit_oid, tree_oid, ts_ms FROM seals LIMIT 0",
        "SELECT oid, kind, reason, ts_ms FROM landmarks LIMIT 0",
        "SELECT oid, commit_oid, agent_id, ts_ms FROM object_intro LIMIT 0",
        "SELECT id, hmac_key, seal_pub FROM cap_root LIMIT 0",
    ];
    for sql in CHECKS {
        conn.prepare(sql).map_err(map_sql)?;
    }
    let legacy_secrets: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cap_root WHERE length(hmac_key) != 0",
            [],
            |row| row.get(0),
        )
        .map_err(map_sql)?;
    if legacy_secrets != 0 {
        return Err(Error::Corrupt(
            "metadata v1 contains legacy root HMAC material".into(),
        ));
    }
    Ok(())
}

fn migrate_metadata(conn: &mut Connection) -> Result<()> {
    let version = metadata_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Invalid(format!(
            "metadata schema v{version} is newer than supported v{SCHEMA_VERSION}"
        )));
    }
    if version == SCHEMA_VERSION {
        return verify_schema_v1(conn);
    }
    if version != 0 {
        return Err(Error::Corrupt(format!(
            "unsupported metadata schema version {version}"
        )));
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql)?;
    tx.execute_batch(SCHEMA).map_err(map_sql)?;
    tx.execute(
        "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
        [],
    )
    .map_err(map_sql)?;
    verify_schema_v1(&tx)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(map_sql)?;
    tx.commit().map_err(map_sql)?;
    Ok(())
}

'''
s = s.replace(impl_marker, helpers + impl_marker, 1)

pattern = re.compile(
    r'    pub fn open\(path: &Path\) -> Result<Self> \{.*?\n    \}\n\n    pub fn set_cap_root',
    re.S,
)
replacement = r'''    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(map_sql)?;
        // Configure connection behavior before starting any migration transaction.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sql)?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(map_sql)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        migrate_metadata(&mut conn)?;
        Ok(Self {
            write: Mutex::new(conn),
        })
    }

    pub fn set_cap_root'''
s, n = pattern.subn(replacement, s, count=1)
if n != 1:
    raise SystemExit(f"Meta::open replacement count={n}")

p.write_text(s)
