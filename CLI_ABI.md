# Forge CLI machine contract

Automation must key on exit codes, not stderr wording.

| Exit | Meaning |
|---:|---|
| 0 | success |
| 1 | denied/capability/input/not-found |
| 2 | corruption or sealed-state violation |
| 3 | transient busy/contention |
| 4 | stale observation or merge conflict |
| 5 | I/O, SQLite, or internal failure |

`--cap PATH|TOKEN` or `FORGE_CAP` is required for normal commands; ForgeFS has no ambient root authority.

## Termination without an exit code

The table above covers every failure ForgeFS can *report*. A caller must also
distinguish termination by signal, which carries no exit code at all. In a shell,
`$?` is then `128 + signum` (`135` for SIGBUS); with `waitpid` use `WIFSIGNALED`.
Treat that as its own outcome: it is not success, and it is not any row above.

One such case is known and is not a ForgeFS defect. ForgeFS keeps its metadata in
SQLite with `journal_mode=WAL`, so SQLite creates the wal-index
`.forge/meta.sqlite-shm`, sparse-extends it to 32768 bytes by writing one byte to
the last byte of each 4096-byte page, and then maps the whole region. Where the
filesystem block size is smaller than the CPU page size -- `mkfs.ext4` chooses
1024 or 2048 automatically for filesystems under about 512 MB -- each of those
one-byte writes allocates only the final block of its page, so a 32 KiB mapping
can be backed by as little as 8 KiB of blocks. A page fault into one of the
remaining holes on a filesystem that can no longer allocate is delivered as
SIGBUS, which is not catchable as an error return.

ForgeFS closes the reachable part of that window: every command checks free space
before SQLite can create the wal-index, and exits 5 with a diagnostic naming the
wal-index when fewer than 32768 bytes are available. Above that threshold SQLite's
own extension writes are all satisfied and the whole mapped region is backed by
the time the connection is usable, so no later fault can find a hole in it. A
residual window remains outside ForgeFS's control -- notably a wal-index that
grows past its first region inside a long operation -- so automation that runs
ForgeFS against a filesystem it does not control must still handle signal
termination. `forge fsck --full` is the correct response; see docs/RECOVERY.md.

A SIGBUS handler is deliberately not installed. The fault arrives on the thread
touching the mapping, inside SQLite, with SQLite's locks and its mapped wal-index
in an indeterminate state; returning from the handler retries the same faulting
access forever, and unwinding out of it would leave the wal-index and the WAL
writer half-updated. Refusing to enter the band is the only safe answer.

`scripts/enospc-sigbus-probe.sh` reproduces and asserts this contract. It needs
Linux, root and `mkfs.ext4`, so it is not part of `scripts/release-gate.sh`.

## Mounts and checkin: `forge mount`, `forge checkin`

Neither verb introduces an exit code. Both map onto the table above.

| Outcome | Exit |
|---|---:|
| mount taken; a `--rw` mount is pinned to the commit its ref holds now (I19) | 0 |
| `--rw` with an `oid:` spec, or a ref that does not hold a commit: refused, because checkin would have nothing to advance (I20) | 1 |
| spec names a ref or object that does not resolve | 1 |
| re-mounting a path at a different spec, or demoting it to read-only, while it holds staged overlay (I19) | 1 |
| the capability may not read the spec, or may not write the ref for `--rw` | 1 |
| `checkin --mount <path>` published (`updated`) or lost the CAS and forked (`forked`) | 0 |
| `checkin --mount <path>` had nothing to publish and the session holds nothing anywhere (`noop`) | 0 |
| `checkin --mount <path>` had nothing to publish while another mount holds staged entries (I22) | 1 |
| `checkin` on a read-only mount, or on a ref the capability may not write | 1 |
| `checkin` of a mount whose ref is protected | 1 |
| `checkin` refused for a stale observation | 4 |

`forge checkin --mount` defaults to `/`. Checkin folds exactly the named mount
and CASes the ref THAT MOUNT names, using that mount's own pinned base as the
expected value; a lost CAS forks (I5/I18) and retargets that mount at the fork.
A session holding read-write mounts on several refs therefore publishes them one
`checkin --mount` at a time, and publishing one never moves what another mount
reads.

A `noop` is therefore a strong statement and callers may rely on it: it means the
session holds no staged work anywhere, not merely that the named mount staged
nothing. Staged work is a property of the namespace, not of one mount -- `forge
abandon session` counts overlay rows across all of them -- so a checkin that
folded only `/` used to answer `noop` with exit 0 for a session whose work sat
under some other read-write mount, and `abandon` then refused the same session as
holding work (#326). A checkin with nothing of its own to publish now refuses
instead, exit 1, naming each other mount and its entry count, because the request
as stated -- "tell me there was nothing to do" -- is unsatisfiable and no retry of
it can succeed (I22). A checkin that DOES publish is unaffected: `updated` and
`forked` are progress and may leave another mount staged, which is exactly how a
session with several writable mounts drains them one `--mount` at a time.
Automation that gets exit 1 from `checkin` re-runs it once per named mount.

## Sealing: `forge seal`

`seal` introduces no exit code. It maps onto the table above:

| Outcome | Exit |
|---|---:|
| the tag was published; with `--attest`, also re-verified from durable bytes | 0 |
| the ref does not exist, the tag name is malformed, or the capability may not seal the ref or `tags/<tag>` | 1 |
| `tags/<tag>` already exists: a published tag is frozen and is never replaced | 2 |
| the ref moved between the read and the publish | 4 |

A seal is a provenance claim about a ref -- "this ref was this commit at this
moment" -- so `seal` compares and swaps against the ref it names, in the same
catalog transaction that publishes the tag (I5, I6). It reads the ref, builds
and signs the snapshot from what it read, and publishes only while the ref
**still holds that commit**. If another agent moved the head in between, the
seal is refused with exit 4 and nothing is published: no tag ref, no reflog
entry, no seals row.

Exit 4, not 1, because a moved head is a stale observation of exactly the kind
`checkin` reports (I8/I9): the request was well formed and authorised, the
caller simply observed a value that is no longer current, and re-reading the ref
makes the same request succeed. An input error would say the opposite -- that no
retry can help.

`seal` never silently seals the new head instead. The caller asked to seal what
they observed; sealing something else under the same tag would publish a claim
nobody made. Automation that gets exit 4 re-reads the ref and decides whether
the new commit is the one it meant to tag.

`--attest` re-reads the published tag from durable bytes (I15), which needs
read authority on `tags/<tag>` as well as seal authority. A capability that may
seal but may not read therefore publishes the tag and then reports the
attestation refusal, exit 1; the tag stays published, because publishing it
succeeded. The daemon's `"attest": true` behaves identically.

Until #331 this window was open: `seal` read the ref, and published whatever it
had read whenever it got there. The resulting tag named a commit the ref no
longer held, `verify` passed on it -- the signed snapshot is internally
consistent, so this was never corruption -- and the caller saw exit 0.
`crates/forge-cli/tests/cli_seal_head_moves.rs` races the window through the
debug-only `FORGEFS_TEST_SEAL_CAS_BARRIER` seam and pins the refusal.

## The daemon surface: `forge serve`

`forge serve` answers the same requests over a unix socket at
`.forge/forge.sock` and, with `--http`, over `POST /v1/<op>`. **The daemon is a
strict projection of this document, not a second ABI.** Three rules define it,
and `crates/forge-api/tests/daemon_abi.rs` is their conformance suite:

1. **Every op is a CLI verb.** The daemon serves a subset of the CLI, never a
   superset. An op that is not in the table below is refused as an input error.
2. **Every field is that verb's own argument, with that verb's own default.** A
   field ForgeFS does not know is refused; it is never accepted and ignored, and
   never silently taken to mean its default.
3. **Every error carries the same classification its exit code carries.** The
   `err.code` string is the exit-code table above under a different name.

| op | CLI verb | body fields |
|---|---|---|
| `session.open` | `session open` | `from` (default `main`) |
| `ns.write` | `write` | `ns`, `path`, and exactly one of `text` or `hex` |
| `ns.read` | `read` | `ns`, `path` |
| `ns.ls` | `ls` | `ns`, `path` (default `/`) |
| `ns.checkin` | `checkin` | `ns`, `mount` (default `/`), `msg` (default empty) |
| `ns.mount` | `mount` | `ns`, `path`, `spec`, `rw` (default `false`) |
| `refs` | `refs` | none |
| `seal` | `seal` | `ref`, `tag`, `attest` (default `false`) |

`hex` is the wire form of `write --file`: a daemon client has no path on the
server's filesystem, so it sends the bytes. Its reach is exactly `--file`'s.

Fields standing for CLI flags (`rw`, `attest`) must be JSON booleans and fields
standing for CLI arguments must be JSON strings; a flag is present or absent, so
`"rw": "true"` is an input error rather than `false`.

Every CLI verb absent from that table -- `import`, `merge`, `branch`, `grant`,
`gc`, `abandon`, `fsck`, `verify`, `export`, `log`, `show`, `stats`, `inbox`,
`landmark`, `init` -- is **not served**, and asking for it is an input error.
Two response details are CLI-only and have no daemon equivalent: the count of
refs suppressed by authority, which `forge refs` writes to stderr, and the
human rendering of `stats`.

`ns.checkin` answers in the CLI's own three-word outcome vocabulary:

```
{"result":"updated","name":"<ref>","oid":"<hex>"}
{"result":"forked","requested":"<ref>","fork":"<ref>","ours":"<hex>","theirs":"<hex>"}
{"result":"noop","name":"<ref>","oid":"<hex>"}
```

Every object id in a daemon response is lowercase hex. Consumers must ignore
response keys they do not know; keys are added, never renamed or removed.

### Daemon error mapping

`err.code` is authoritative. The HTTP status is a coarser view of the same
failure and several classes share one status, so a client that needs the CLI's
classification reads `err.code`, never the status.

| `err.code` | CLI exit | HTTP |
|---|---:|---:|
| — (`"ok": true`) | 0 | 200 |
| `denied` | 1 | 403 |
| `not_found` | 1 | 404 |
| `invalid` | 1 | 400 |
| `invalid_base` | 1 | 409 |
| `sealed` | 2 | 409 |
| `corrupt` | 2 | 500 |
| `busy` | 3 | 503 |
| `stale_observation` | 4 | 409 |
| `conflict` | 4 | 409 |
| `internal` | 5 | 500 |

The adopted rule holds here too: `internal` must be unreachable from
caller-controlled input, so no request a client can shape produces HTTP 500.

The capability is loaded before the op is dispatched, so an unauthenticated peer
cannot use the daemon to discover which ops exist. `serve` requires exclusive
cell ownership (`Forge::open_for_serve`); a daemon and a direct CLI client never
share a repository, and `serve` refuses with `busy` (exit 3) if one is already
there. Transport limits -- frame size, worker pool, admission and read deadlines
-- are in `crates/forge-api/src/serve.rs` and are not part of this contract.

## Reclamation: `forge abandon` and `forge gc`

Neither verb introduces an exit code. Both map onto the table above:

| Outcome | Exit |
|---|---:|
| `abandon fork` retired the ref; `abandon session` retired the namespace | 0 |
| ref or namespace does not exist | 1 |
| ref is outside `forks/`, already abandoned, still mounted, still a session's live ref, or the session holds staged work without the explicit discard flag | 1 |
| ref is protected, or the capability may not write it | 1 |
| ref is sealed | 2 |
| `gc --dry-run` produced a report | 0 |
| `gc --collect` reclaimed (possibly zero) objects | 0 |
| `gc` with neither `--dry-run` nor `--collect`, or with both | 1 |
| `gc --collect --min-age-secs` below the hard floor | 1 |
| `gc` under a ref-scoped capability | 1 |
| `gc` could not prove reachability because an object is unreadable or does not decode | 2 |

`forge gc --dry-run` **never deletes** and is the reporting half. `forge gc
--collect` is the reclaiming half: it unlinks unreachable objects and removes
the catalog rows that named them. Exactly one of the two flags is required, so
a bare `forge gc` still exits 1 with the diagnostic pointing at `docs/GC.md`,
and no invocation deletes anything by default. `--min-age-secs` is refused
below its hard floor rather than quietly raised, because that floor is the only
bound ForgeFS has on the window between a writer's put and the transaction that
names it. `docs/GC.md` states the root set, the invariant collection preserves
(I23) and the one precondition it cannot prove for itself.

`forge gc --json` writes one JSON object to stdout. It is not part of the
`forge stats --json` contract above and carries no `schema_version`; consumers
must ignore keys they do not know and must not assert amounts.

## Structured metrics: `forge stats --json`

`forge stats --json` writes exactly one JSON object to stdout and exits 0. It
introduces no exit code: a missing capability, an unreadable repository, or a
corrupt catalog are reported by the table above, unchanged.

The document is:

```
schema_version   integer, currently 2
scope            "process-lifetime"
note             prose restating scope
durability       journal_mode, synchronous, fullfsync, read_only
store            puts, dedup_hits, fsync_file, fsync_file_us,
                 fsync_dir, fsync_dir_us, barrier_fs, barrier_fs_us,
                 barrier_fs_batches, barrier_us
sqlite           txn_count, txn_us, explicit_txn_count, lock_acquires,
                 lock_wait_us, write_lock_acquires, write_lock_wait_us,
                 read_lock_acquires, read_lock_wait_us, busy, cas_updated,
                 cas_forked, cas_denied, cas_noop, accounted_us
api              sessions_opened, stale_observation, merge_applied, merge_conflict
```

Stability rules for consumers:

- Keys are added, never renamed or removed, while `schema_version` is 2. A
  consumer must ignore keys it does not know. `barrier_fs`, `barrier_fs_us`
  and `barrier_fs_batches` were added this way.
- `fsync_dir` counts per-directory barriers and `barrier_fs` counts
  filesystem-wide ones, which the object store may take instead when its
  directory-barrier policy is `collapsed`. Neither is the directory-barrier
  total on its own; their sum is. `barrier_fs_batches` counts the batches a
  filesystem-wide barrier satisfied, leader and followers alike, so
  `barrier_fs_batches / barrier_fs` is the achieved sharing depth and is never
  a barrier count. `barrier_us` is the saturating sum of all three duration
  fields.
- `txn_count` is every write transaction SQLite committed on the catalog: each
  explicit `BEGIN IMMEDIATE` that committed, and each autocommit statement that
  wrote, since SQLite gives every such statement its own implicit transaction.
  `explicit_txn_count` is the explicit half alone and is the only sample count
  that pairs with `txn_us`; `txn_count / txn_us` is not an average.
- `lock_acquires` / `lock_wait_us` sum the write connection's mutex and the
  read pool's slot mutexes, so they measure neither family on its own. Use
  `write_lock_acquires` / `write_lock_wait_us` for writer contention and
  `read_lock_acquires` / `read_lock_wait_us` for the pool.
- **Schema 1 -> 2 (issue #311).** No key was renamed or removed, but under
  schema 1 `txn_count` counted only explicit transactions, so a read-heavy
  phase reported `0` while the catalog committed one autocommit write per
  operation. A consumer comparing a schema-1 series to a schema-2 series is
  comparing two different quantities; `explicit_txn_count` is the field that
  continues the old series.
- Every counter is a non-negative integer. `barrier_us` and `accounted_us` are
  saturating sums of the components printed beside them, not wall time.
- **`scope` is the whole contract.** Every counter is a cumulative total for
  the single process that ran the command, from its repository open until the
  snapshot. It is not per-operation, not per-checkin, and not a benchmark. A
  one-shot `forge stats` therefore reports little more than its own open; the
  totals are meaningful for a long-lived embedder calling
  `Forge::stats_report()`, and the per-checkin cost mix remains unavailable
  (`docs/BENCH.md`).
- Values are environment-dependent. Automation may assert the shape; it must
  not assert amounts.

`forge stats` without `--json` renders the same numbers for humans and is not
part of this contract.

