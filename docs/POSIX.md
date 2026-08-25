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

The price was that ForgeFS could not import *any* source tree that contains a
symlink, which is most real ones, and there was no opt-out flag. That is a
usability problem, not a correctness one, and it is the right way round -- but
it was still an adoption blocker, so this pass added the opt-out **without**
touching the frozen format:

* the default is unchanged and still refuses, but one run now names **every**
  symlink in the tree instead of failing on the first;
* `forge import --follow-symlinks` (API: `import_dir_with` with
  `ImportOptions { follow_symlinks: true }`) materialises each link as the
  **content** of its target -- a file link becomes a regular Blob under the
  link's name, a directory link becomes a Tree. It is lossy, it is opt-in, and
  the refusal says so;
* **containment is not optional.** A followed target that resolves outside the
  canonical import root is refused, naming the link and where it pointed. This
  is the security half: a source tree is untrusted input, and without the rule
  a link inside it could name `/etc/passwd` or `../../` and copy host bytes
  into a content-addressed, shareable, sealable store. Dangling links and every
  loop shape are refused too, so no input can hang or unbound the recursion.

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
| Symlink cycle `a -> b -> a` | exit 1, same message; no hang, no unbounded recursion | none | **REFUSED, loudly** |
| Symlink to `../` outside the root | exit 1, same message | none | **REFUSED, loudly** |
| Import root *is* a symlink | exit 0, dereferenced | contents of the target | **FOLLOWED** (the operator named that path; see below) |

The six shapes above were re-measured at `v0.2.1` before any design work, on
the shipped binary, precisely because the answer decides the task: had import
*followed* links, the absolute and `../` cases would have been a path escape
rather than a missing feature. They are refused, so this is a feature gap.

With `--follow-symlinks`, the same fixtures behave like this:

| Host input under `--follow-symlinks` | `forge import` | Result |
|---|---|---|
| Relative link to a sibling file | exit 0 | regular Blob under the link's name, target's bytes |
| Link to a directory inside the root | exit 0 | Tree under the link's name, target's subtree |
| Absolute link to `/etc/passwd` | exit 1 `... target /etc/passwd is outside the import root ...` | nothing published |
| Link to `../outside` (dir or file) | exit 1, same shape | nothing published |
| Link chain that hops inside then leaves | exit 1, same shape | nothing published |
| Sibling root sharing a textual path prefix | exit 1, same shape (containment is component-wise) | nothing published |
| Dangling link | exit 1 `... target does not exist` | nothing published |
| `a -> b -> a` | exit 1 (ELOOP, surfaced as an input error) | nothing published |
| `link -> .`, or mutual `x/toy -> ../y`, `y/tox -> ../x` | exit 1 `... re-enters a directory already being imported (symlink loop)` | nothing published |

Every one of those is `Error::Invalid`, i.e. exit 1. A source tree is
caller-controlled input and must never reach the internal-failure exit code.
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

### Open items this measurement raised

1. **The root-symlink asymmetry.** CLOSED. `import_dir` now canonicalises the
   import root before walking it, so `forge import ./link-to-tree` and
   `forge import ./real-tree` produce identical content, and the resolved real
   path is what containment is measured against -- a symlinked root does not
   widen the sandbox. The root is still followed, because it is the operator's
   own argument rather than untrusted tree content.
2. **Directory descent has no symlink re-verification.** NARROWED. Each
   directory is now opened `O_DIRECTORY|O_NOFOLLOW` before enumeration, so a
   dirent that says "directory" but has become a symlink is refused outright
   (`ELOOP`/`ENOTDIR` map to `source path changed type during import`), and the
   directory's `(dev, ino)` is re-checked against the pathname after the
   children are processed. That is detection at both endpoints, in the same
   spirit as `read_import_file`; it is not a host snapshot primitive, and a
   swap that races strictly between the open and the `read_dir` is still
   theoretically possible. Full `openat`-relative descent remains the complete
   fix and remains a larger change.
3. **One symlink per attempt.** CLOSED. The refusal sweeps the tree
   (lstat-only, and only on the already-failing path, so a successful import
   pays nothing) and names up to 32 symlinks in one diagnostic.
4. **No bound on materialised sparse content.** STILL OPEN. Nothing sits
   between a host file's apparent length and the object store.

## Why `--follow-symlinks` and not a format change

Three options were on the table and only one of them can land under a frozen
VERSION 1.

**(a) Encode a symlink inside v1 without changing how existing readers parse
bytes -- impossible.** A `TreeEntry` is `{name, oid, kind, exec}` with `kind` in
`{Blob, Tree}`. There is no spare field and no spare bit. Reusing `exec` is
worse than useless: a v0.2.1 reader would materialise the link's target *path
text* as an executable regular file, which is silent corruption and strictly
worse than today's refusal. Adding a third `kind` changes Tree header bytes, and
FORMAT.md's typed decoder rejects unknown values, so an existing reader fails
closed on the new object.

**(b) Propose VERSION 2 -- real, but not this PR.** FORMAT.md's release
boundary is explicit: after a VERSION 1 binary is released, a change that makes
that released reader unable to interpret newly written objects "requires a new
repository VERSION". `v0.2.1` is a release tag, so a symlink entry kind is a
VERSION bump, and a VERSION bump is not a local change to `import.rs`. It
touches the `.forge/VERSION` gate, every typed decoder, export, mount, merge and
diff, the frozen canonical fixtures in `testdata/canonical/`, GC reachability,
and -- the part that makes it a maintainer decision rather than a contributor
one -- `verify` and the ed25519 sealed releases, because a v1 binary cannot
verify a v2 repository's snapshots at all. The smallest honest shape is still
the Git `120000` model: a VERSION 2 Tree entry with an explicit kind byte
carrying `Symlink`, target held as the Blob payload. This document proposes it;
this change does not implement it and does not touch FORMAT.md.

**(c) Make import SAFE and EXPLICIT under v1 -- shipped.** Refuse by default,
name every offending path, and offer an opt-in that materialises contained
targets with escape prevention. No object bytes change, no ObjectId moves, no
reader is affected, and CLI_ABI exit codes are unchanged: every new refusal is
`Error::Invalid`, which was already exit 1.

What this deliberately does NOT claim: `--follow-symlinks` is not symlink
support. It is a lossy conversion that the operator asks for by name, and a
tree imported that way does not round-trip to the source it came from. When a
future VERSION can represent a link, the refusal becomes a representation and
this flag becomes a compatibility shim.

## What changed in this pass

`crates/forge-api/src/import.rs`:

* `ImportOptions { follow_symlinks }` and `Forge::import_dir_with`;
  `import_dir` keeps its signature and its refusing default.
* The import root is canonicalised once and becomes the containment root.
* Directory descent opens `O_DIRECTORY|O_NOFOLLOW` and re-verifies
  `(dev, ino)` against the pathname after enumeration.
* A `(dev, ino)` stack of the directories on the current recursion path detects
  symlink loops; `MAX_IMPORT_DEPTH` is a backstop for pathological real trees.
* The default refusal sweeps the tree once, on the failing path only, so one
  run names every symlink.

`crates/forge-cli/src/main.rs` gains `forge import --follow-symlinks`.

No object format, no tree encoding, no ObjectId, and no exit code changed.
FORMAT.md is untouched. `crates/forge-api/tests/import_symlinks.rs` pins the
containment rule, the loop and dangling refusals, the one-pass report, and the
root/target agreement; the pre-existing characterisation tests in
`posix_adapter_characterisation.rs` still pass unmodified.
