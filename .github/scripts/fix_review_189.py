from pathlib import Path


def require_once(text: str, needle: str, label: str) -> None:
    n = text.count(needle)
    if n != 1:
        raise SystemExit(f"{label}: expected one match, found {n}")


# Preflight every marker before any write, so a drifted/rerun script is atomic.
api = Path("crates/forge-api/src/lib.rs")
api_s = api.read_text()
old_publish = r'''fn publish_noreplace(from: &Path, to: &Path) -> Result<()> {
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

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
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
'''
require_once(api_s, old_publish, "publish_noreplace")

meta = Path("crates/forge-store/src/meta.rs")
meta_s = meta.read_text()
require_once(meta_s, "const CURRENT_SCHEMA_VERSION: i64 = 1;", "schema version visibility")
old_meta_prefix = r'''    pub fn open(path: &Path) -> Result<Self> {
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
'''
require_once(meta_s, old_meta_prefix, "Meta::open prefix")

store_lib = Path("crates/forge-store/src/lib.rs")
store_s = store_lib.read_text()
old_export = "pub use meta::{sanitize_agent, Meta, MetaStats, MountRow, NsRow, OverlayRow};"
require_once(store_s, old_export, "schema version re-export")

tests = Path("crates/forge-store/tests/schema_migrations.rs")
test_s = tests.read_text()
require_once(test_s, "use forge_store::Meta;", "schema test import")
old_insert = r'''    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (2, 0)",
        [],
    )
    .unwrap();'''
require_once(test_s, old_insert, "newer schema test insert")
old_future_setup = r'''    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);\
         INSERT INTO schema_migrations (version, applied_ms) VALUES (2, 0);",
    )
    .unwrap();'''
require_once(test_s, old_future_setup, "side-effect test setup")

new_publish = r'''fn publish_noreplace(from: &Path, to: &Path) -> Result<()> {
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
        return Err(error.into());
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = (from_c, to_c);
        Err(Error::Invalid(
            "atomic no-replace repository publication is unsupported on this platform".into(),
        ))
    }
}
'''
api.write_text(api_s.replace(old_publish, new_publish, 1))

new_meta_prefix = r'''    pub fn open(path: &Path) -> Result<Self> {
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
'''
meta_s = meta_s.replace("const CURRENT_SCHEMA_VERSION: i64 = 1;", "pub const CURRENT_SCHEMA_VERSION: i64 = 1;", 1)
meta.write_text(meta_s.replace(old_meta_prefix, new_meta_prefix, 1))

store_lib.write_text(
    store_s.replace(
        old_export,
        "pub use meta::{sanitize_agent, Meta, MetaStats, MountRow, NsRow, OverlayRow, CURRENT_SCHEMA_VERSION};",
        1,
    )
)

test_s = test_s.replace(
    "use forge_store::Meta;",
    "use forge_store::{Meta, CURRENT_SCHEMA_VERSION};",
    1,
)
test_s = test_s.replace(
    old_insert,
    r'''    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, 0)",
        [CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();''',
    1,
)
test_s = test_s.replace(
    old_future_setup,
    r'''    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_ms) VALUES (?1, 0)",
        [CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();''',
    1,
)
tests.write_text(test_s)
