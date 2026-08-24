-- A version-0 catalog whose `refs` relation predates a column the current
-- schema requires. This shape is SYNTHETIC: no ForgeFS release ever shipped
-- it. It exists because `CREATE TABLE IF NOT EXISTS` is a no-op against a
-- relation that already exists with a different shape, so a migration from a
-- pre-versioning catalog can silently fail to reach the target schema. The
-- migration must refuse rather than record a version it did not reach.
CREATE TABLE refs (
  name       TEXT PRIMARY KEY,
  oid        BLOB NOT NULL CHECK(length(oid)=32),
  kind       TEXT NOT NULL CHECK(kind IN ('commit','tree','conflict','snapshot')),
  protected  INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL
);

INSERT INTO refs (name, oid, kind, protected, updated_ms) VALUES
  ('main', X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'commit', 0, 1700000000001);
