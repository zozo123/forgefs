# POSIX adapter semantics (issue #20)

A ForgeFS `TreeEntry` is exactly `{name, kind, id, exec}`. FORMAT.md freezes
that encoding for repository VERSION 1, so the adapter between a host directory
and a ForgeFS Tree has to decide, for every piece of POSIX metadata that has no
field, whether to preserve it, convert it, drop it, or refuse.

This document is the measured answer, then the deliberate one. Everything in
the first table was run against the shipped `forge` binary; every row is pinned
by `crates/forge-api/tests/posix_adapter_characterisation.rs` so a change to any
of it is a deliberate act with a failing test attached.

## The headline

**Import does not follow symlinks.** It refuses them, by name, with exit 1. The
feared silent-data-loss case for issue #20 -- import dereferences a link, stores
the target as a regular file, and export then produces a tree that is not the
tree that was imported -- **does not happen**. I2 is intact at this boundary.

The price is that ForgeFS cannot import *any* source tree that contains a
symlink, which is most real ones, and there is no opt-out flag. That is a
usability problem, not a correctness one, and it is the right way round.

## Measured behaviour

Method: `forge init`, build a fixture directory, `forge import`, `forge export`,
extract, compare. Binary at `63fa098` (the #315 `perf(meta)` commit), Linux
ext4-backed overlay, non-root.

| Host input | `forge import` | Round-trip result | Verdict |
|---|---|---|---|
| Relative symlink | exit 1 `import refuses symlink <path>` | none | **REFUSED, loudly** |
| Absolute symlink | exit 1, same message | none | **REFUSED, loudly** |
| Dangling symlink | exit 1, same message | none | **REFUSED, loudly** |
| Symlink to a directory | exit 1, same message | none | **REFUSED, loudly** |
| Import root *is* a symlink | exit 0, dereferenced | contents of the target | **SILENTLY FOLLOWED** (asymmetry) |
| FIFO | exit 1 `import refuses unsupported file type <path>` | none | **REFUSED, loudly** |
| Unix socket | exit 1, same message | none | **REFUSED, loudly** |
| Non-UTF-8 name | exit 1 `non-utf8 name in <dir>` | none | **REFUSED, loudly** |
| Hardlinked pair a.txt/b.txt | exit 0, one deduplicated Blob | two independent regular files | **SILENTLY CONVERTED** |
| Mode `0600` | exit 0, `exec=false` | extracted `0644` | **SILENTLY WIDENED** |
| Mode `0444` | exit 0, `exec=false` | extracted `0644` | **SILENTLY WIDENED** |
| Mode `0741` | exit 0, `exec=true` | extracted `0755` | **SILENTLY WIDENED** |
| Mode `0755` | exit 0, `exec=true` | extracted `0755` | preserved |
| Mode `4755` setuid | exit 0, `exec=true` | extracted `0755` | **SILENTLY DROPPED** (safe direction) |
| Mode `2755` setgid | exit 0, `exec=true` | extracted `0755` | **SILENTLY DROPPED** (safe direction) |
| Mode `0700` directory | exit 0 | extracted `0755` | **SILENTLY WIDENED** |
| Mode `1777` sticky directory | exit 0 | extracted `0755` | **SILENTLY DROPPED** |
| Mode `0000` (unreadable) | exit 5, `io: <path>: Permission denied` | none | REFUSED (path added by this change) |
| mtime 2001-02-03 | exit 0, not recorded | extracted mtime `0` | **SILENTLY DROPPED** |
| 100 MiB sparse file, 0 blocks | exit 0 | 100 MiB of literal zeros | **SILENTLY MATERIALISED** |
| `.git` at the import root | exit 0, skipped | absent from the export | **SILENTLY DROPPED** |
| `.git` one level down | exit 0 | preserved | preserved (user data) |
| uid / gid | never read | `0/0` in the archive | **SILENTLY DROPPED** |
| xattrs, ACLs | never read | absent | **SILENTLY DROPPED** |

Three numbers worth keeping:

- A 4.0 KiB source directory holding one 100 MiB sparse file produced **101 MiB
  of objects and a 101 MiB archive**: roughly 25000x space amplification, with
  no warning and no size guard anywhere in the path.
- `0600 -> 0644` is a **widening**. A file that was owner-only going in comes
  back group- and world-readable. Nothing in the pipeline reports it.
- Before the fix in this change, an unreadable source file failed the whole
  import with `io: Permission denied (os error 13)` and **no path at all**, so
  on a large tree there was no way to learn which file was the problem.

## Proposed semantics

### Representable in the frozen v1 encoding -- keep as is

`exec` is the one POSIX bit the format has, and import already derives it as
"any of `0o111` is set". Export materialises `0644`/`0755` from it. That is the
Git model, it is deterministic, and it should stay. **Document the widening
rather than refuse it**: refusing every file that is not already `0644` or
`0755` would reject ordinary source trees, and the mode loss is recoverable by
the operator (a `chmod` after extraction) in a way that a lost symlink is not.

The real gap here is not the semantics, it is that nobody is told. This file
and the export path are where that belongs.

### Needs a format change -- **cannot land now**

| Metadata | Why v1 cannot hold it |
|---|---|
| Symlink | No entry kind for it. `EntryKind` is `{Blob, Tree}` and a Tree entry has no mode word, so there is nowhere to say "these Blob bytes are a link target". Needs a third `EntryKind` or a mode field, and **both change Tree bytes and therefore every ObjectId**. |
| Full mode, setuid, setgid, sticky | Same reason: no mode field on a Tree entry. |
| mtime | Same, and it would make a Tree non-deterministic for identical content, which is a direct attack on I2. |
| Hardlink identity | Needs an alias or inode notion the object model does not have. Content dedup already gives the storage benefit; only the aliasing is lost. |
| uid/gid, xattrs, ACLs | Not representable, and arguably should never be: they are host-local authority, not content. |

Per FORMAT.md, adding an entry kind or a Tree-entry field "changes framing or
canonical encoding in a way that changes identity" and so **requires a new
repository VERSION**. This is a DESIGN item, not a patch. If symlinks are
wanted, the smallest honest shape is a VERSION 2 Tree entry with an explicit
kind byte carrying `Symlink` alongside `Blob` and `Tree`, the link target held
as the Blob payload, which is the Git `120000` model. None of this should be
smuggled into VERSION 1.

### Should stay an explicit refusal

Symlinks, FIFOs, sockets, device nodes and non-UTF-8 names. Refusing loudly is
correct and already implemented: the alternative is an export that differs from
its import, which is exactly the silent violation I2 exists to prevent. When a
future VERSION represents symlinks, the refusal becomes a representation; until
then it must not become a conversion.

### Open items this measurement raises

1. **The root-symlink asymmetry.** `forge import ./link-to-tree` dereferences;
   `forge import ./parent` refuses because `parent/link-to-tree` is a symlink.
   Both are defensible in isolation, but they disagree about the same link.
   Deciding this is cheap and needs no format change.
2. **Directory descent has no symlink re-verification.** `read_import_file`
   opens with `O_NOFOLLOW` and re-checks `dev`/`ino` after reading, matching the
   stated "bounded TOCTOU detection". Directory descent has no equivalent:
   `import_walk` trusts the dirent file type and then calls `fs::read_dir` on
   the path, which follows symlinks. A directory replaced by a symlink between
   those two steps would be walked, so content outside the import root could
   enter the tree despite the symlink refusal. Closing this properly wants an
   `openat`-style descent (an fd opened `O_DIRECTORY|O_NOFOLLOW`, read through
   that fd), which is a larger change than this one.
3. **One symlink per attempt.** Import fails on the first unsupported entry, so
   preparing a real tree is a fix-one-rerun loop. Reporting every unsupported
   entry in a single pass would cost nothing in format terms.
4. **No bound on materialised sparse content.** Nothing sits between a host
   file apparent length and the object store.

## What changed in this pass

Only the diagnostic. `crates/forge-api/src/import.rs` now names the path in
every host I/O error it raises (`io_at`), keeping `Error::Io` so the CLI_ABI
exit code for a host I/O failure is unchanged. No object format, no tree
encoding, and no accept-or-refuse decision was altered.
