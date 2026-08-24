-- ForgeFS catalog at metadata schema version 0: a pre-versioning catalog.
--
-- Version 0 is not an invented past release. `schema_version()` in
-- crates/forge-store/src/meta.rs defines 0 as "no schema_migrations ledger",
-- and `migrate` carries an explicit 0 -> 1 step for exactly this state. These
-- bytes are frozen evidence for that step, not a snapshot to regenerate when a
-- test fails. See testdata/schema/README.md before touching them.

CREATE TABLE refs (
  name       TEXT PRIMARY KEY,
  oid        BLOB NOT NULL CHECK(length(oid)=32),
  kind       TEXT NOT NULL CHECK(kind IN ('commit','tree','conflict','snapshot')),
  protected  INTEGER NOT NULL DEFAULT 0,
  sealed     INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL
);

CREATE TABLE reflog (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  old_oid BLOB,
  new_oid BLOB NOT NULL,
  agent_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  ts_ms INTEGER NOT NULL
);
CREATE INDEX reflog_name ON reflog(name, id);

CREATE TABLE namespaces (
  id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  pinned_oid BLOB,
  live_ref TEXT
);

CREATE TABLE observations (
  ns_id TEXT NOT NULL,
  mount TEXT NOT NULL,
  path  TEXT NOT NULL,
  oid   BLOB NOT NULL CHECK(length(oid)=32),
  PRIMARY KEY (ns_id, mount, path)
);

CREATE TABLE mounts (
  ns_id TEXT NOT NULL,
  path  TEXT NOT NULL,
  spec  TEXT NOT NULL,
  mode  TEXT NOT NULL CHECK(mode IN ('ro','rw')),
  PRIMARY KEY (ns_id, path)
);

CREATE TABLE overlay (
  ns_id    TEXT NOT NULL,
  mount    TEXT NOT NULL,
  path     TEXT NOT NULL,
  blob_oid BLOB,
  exec     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (ns_id, mount, path)
);

CREATE TABLE seals (
  tag        TEXT PRIMARY KEY,
  snap_oid   BLOB NOT NULL UNIQUE,
  commit_oid BLOB NOT NULL,
  tree_oid   BLOB NOT NULL,
  ts_ms      INTEGER NOT NULL
);

CREATE TABLE landmarks (
  oid     BLOB PRIMARY KEY,
  kind    TEXT NOT NULL,
  reason  TEXT NOT NULL,
  ts_ms   INTEGER NOT NULL
);

CREATE TABLE object_intro (
  oid        BLOB PRIMARY KEY,
  commit_oid BLOB NOT NULL,
  agent_id   TEXT NOT NULL,
  ts_ms      INTEGER NOT NULL
);

CREATE TABLE cap_root (
  id INTEGER PRIMARY KEY CHECK(id=1),
  hmac_key BLOB NOT NULL DEFAULT X'',
  seal_pub BLOB NOT NULL
);

INSERT INTO refs (name, oid, kind, protected, sealed, updated_ms) VALUES
  ('main', X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'commit', 1, 0, 1700000000001),
  ('snap/one', X'b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2', 'snapshot', 0, 1, 1700000000002);

INSERT INTO reflog (id, name, old_oid, new_oid, agent_id, reason, ts_ms) VALUES
  (1, 'main', NULL, X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'agent-a', 'import', 1700000000001),
  (2, 'snap/one', NULL, X'b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2', 'agent-a', 'seal', 1700000000002);

INSERT INTO namespaces (id, agent_id, created_ms, pinned_oid, live_ref) VALUES
  ('ns-fixture-0001', 'agent-a', 1700000000000, X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'main');

INSERT INTO observations (ns_id, mount, path, oid) VALUES
  ('ns-fixture-0001', '/src', 'a.txt', X'd4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4');

INSERT INTO mounts (ns_id, path, spec, mode) VALUES
  ('ns-fixture-0001', '/src', 'main', 'rw');

INSERT INTO overlay (ns_id, mount, path, blob_oid, exec) VALUES
  ('ns-fixture-0001', '/src', 'b.txt', X'd4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4', 0);

INSERT INTO seals (tag, snap_oid, commit_oid, tree_oid, ts_ms) VALUES
  ('v1', X'b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2', X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', X'c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3', 1700000000002);

INSERT INTO landmarks (oid, kind, reason, ts_ms) VALUES
  (X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'commit', 'fixture', 1700000000001);

INSERT INTO object_intro (oid, commit_oid, agent_id, ts_ms) VALUES
  (X'e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5', X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'agent-a', 1700000000001);

-- A pre-versioning catalog may still carry a root HMAC key. Opening writable
-- scrubs it (I14: no ambient root), which is the one row this fixture expects
-- to differ after migration.
INSERT INTO cap_root (id, hmac_key, seal_pub) VALUES
  (1, X'9999999999999999999999999999999999999999999999999999999999999999', X'5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e');
