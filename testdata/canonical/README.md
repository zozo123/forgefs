# Canonical VERSION 1 fixtures

These files are compatibility vectors, not snapshots to regenerate when a
test fails. `*.hex` freezes complete object-file bytes; `*.oid` freezes the
BLAKE3-256 identity of those bytes.

The Commit fixtures deliberately cover both wire shapes:

- `commit.hex` / `commit.oid`: the `contrib` key is absent and decodes as
  `None`;
- `commit_with_contribution.hex` / `.oid`: `contrib` is present as a 32-byte
  Contribution ObjectId and therefore changes Commit identity.

Changing an existing fixture requires preserving the old fixture and reader,
then defining an explicit repository VERSION transition. A mutable SQLite
schema migration is never a reason to rewrite these bytes.
