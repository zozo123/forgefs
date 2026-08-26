# Path-name portability at the archive boundary

ForgeFS treats path identity as data. Repository names are canonical UTF-8 byte
strings and ForgeFS does **not** case-fold or Unicode-normalize them on write,
read, import, merge, or export. Two distinct repository names must never become
one name merely because a host filesystem would consider them equivalent (I16).

## What `forge export` guarantees

`forge export` writes a tar archive. Tar member names preserve the exact UTF-8
spelling stored in the ForgeFS Tree; ForgeFS does not rewrite an NFD spelling to
NFC, change case, or otherwise make member names host-native.

Before emitting the archive, export recursively checks sibling names with the
portability fold used by `export_name_collisions.rs` (case folding plus Unicode
NFC). If two siblings collapse to the same portable spelling, export refuses
before publishing a partial artifact. `--allow-name-collisions` is the explicit
operator opt-out for workflows that knowingly need such an archive. The opt-out
does not normalize either member; it preserves both exact tar names.

This fail-closed collision check protects the destructive two-name case: a
normalizing or case-insensitive target must not silently overwrite one ForgeFS
sibling because ForgeFS produced an unsafe archive by default.

## What extraction cannot guarantee

Extraction is outside the ForgeFS boundary. A third-party tar extractor and the
target filesystem may change the spelling of even a **single** member name while
materializing it. For example, a host may present a canonically equivalent
Unicode spelling or apply other filesystem-specific name rules. No sibling has
to collide for the resulting path bytes to differ from the repository bytes.

Therefore:

- the ForgeFS artifact of record is the tar member name, not the spelling a
  third-party extractor later materializes;
- extracting an archive onto a normalizing filesystem is not a byte-for-byte
  path-identity guarantee, even when ForgeFS reported no sibling collision;
- re-importing a materialized tree may legitimately produce a different Tree
  ObjectId if the host changed a name's bytes;
- applications that require exact path identity should inspect/transport the
  archive itself or materialize on a filesystem whose name semantics they have
  verified.

ForgeFS will not "fix" this by normalizing repository names or tar member names:
that would move identity silently and would turn a host portability property
into repository semantics.

## Future native directory export

If ForgeFS gains an `export-directory` adapter, the archive boundary no longer
applies. That adapter must treat materialization as part of the trusted
operation: after creating each directory entry it must re-read the host-visible
name (or use an equivalent descriptor-relative proof) and refuse if the bytes do
not equal the requested ForgeFS name. A host-normalized spelling is an error,
not a successful export with a warning.

Related evidence: `crates/forge-api/tests/export_name_collisions.rs`, path
identity tests, `docs/POSIX.md`, issue #372, and I16.
