# Reclamation: `abandon` and `gc`

Issues #12 and #309. #309 argues that garbage collection and the missing
`abandon` verb are one problem, and this is that argument implemented as far as
it can honestly go: **`abandon` ships, `gc --dry-run` ships, collection does
not.**

## The problem

I18 says a refused checkin never destroys staged work, so a losing CAS mints
`forks/<ref>/<agent>/<ulid>` and repoints the losing session at it. One measured
512-writer contended round produced 1546 refs, 511 of them forks. Every fork
pins an object closure and nothing ever reaped one. That is unbounded growth on
the steady-state path, which is the path ForgeFS exists to serve.

The obvious fix is the wrong one. A reachability sweep that treats a fork as
ordinary garbage deletes exactly the work I18 promises to keep. Fork refs are
not litter; they are the receipt for a contribution that lost a race.

## The resolution

A fork is a **GC root until it is explicitly resolved**, and there are two ways
to resolve one:

* **merged** — implicit. The fork's objects become reachable from the target
  ref, so the fork ref roots nothing the merge did not already root.
* **abandoned** — explicit, and the verb this change adds.

Because `abandon` removes the `refs` row, the rule "unresolved forks are roots"
needs no special case anywhere in the root computation. It falls out of "every
ref is a root".

## The root set

```
refs                       every row in `refs`, including unresolved forks/*
live session pins          namespaces.pinned_oid
session mounts             raw-oid mounts (a `ref:` mount roots nothing new)
session overlays           staged blobs that have not been checked in
session observations       blob/tree ids a session recorded reading (I9)
landmarks                  #249 made `landmark` ref-unrestricted precisely
                           because landmarks become load-bearing here
seals                      snapshot, commit and tree of every sealed tag
```

## `abandon`

```
forge abandon fork  forks/<ref>/<agent>/<ulid>
forge abandon session <ns> [--discard-staged]
```

`abandon fork` is the only operation in ForgeFS that deletes a `refs` row. Four
properties make that safe rather than a violation of I18:

1. **It is a deliberate act, not a failure path.** I18 constrains what happens
   when a checkin is *refused*. Refusal still forks and still keeps the work.
2. **The reflog is the tombstone.** The row is replaced by a terminal reflog
   entry with `reason = 'abandon'` carrying the retired OID, the agent and the
   timestamp. The work stays addressable by OID and auditable by name until a
   collector that does not yet exist removes the bytes.
3. **It refuses while anyone can still resolve the name.** Protected, sealed,
   named by a namespace's `live_ref`, or named by any mount — all four are
   checked inside the same immediate transaction that would delete the row.
   Those are exactly the states in which a dangling reference could appear, and
   a dangling mount is what `fsck` reports as corruption (exit 2).
4. **The name is retired for good.** Recreating it would append a fresh
   `old_oid IS NULL` reflog row on top of a chain that already terminated, which
   `audit_catalog` correctly reports as REFLOG_CHAIN corruption. Every
   ref-creation path now refuses a retired name. Fork names carry a ULID, so no
   legitimate caller ever needs one back.

`fsck --full` previously reported any reflog name without a `refs` row as
REFLOG_ORPHAN. It now accepts a chain that terminates in `abandon`, and only
that shape.

`abandon session` is the escape hatch a stranded session never had: it drops the
namespace's pin, mounts, overlay and observations from the root set. A session
still holding staged overlay entries is **refused** unless `--discard-staged` is
passed, so no accidental path destroys staged work. The session's `heads/` ref
is deliberately left alone — that is published history, not fork churn.

`abandon fork` needs write authority over the ref itself, so the agent that
forked can retire its own fork with no broader grant.

## `gc --dry-run`

```
forge gc --dry-run [--min-age-secs N] [--json]
```

Reports what a collector would reclaim and deletes nothing. `--dry-run` is
mandatory; without it the command exits 1 with a diagnostic pointing here.

Two properties make the *plan* sound:

* the object directory is scanned **before** the roots are read. An object
  created after the scan is therefore never a candidate, and an object that
  becomes reachable between the scan and the walk is seen by the walk.
* the reachability walk **fails closed**. `fsck` records an unreadable or
  undecodable object as a finding and carries on, which is right for a report.
  Here it would silently drop a subtree from the reachable set, so `gc` aborts
  the whole computation with `Corrupt` instead.

`--min-age-secs` (default 86400) withholds unreachable objects that are too
young to be provably garbage. An object is fsynced into `objects/` *before* the
catalog row that roots it — I4 requires that order — so there is always a window
in which live bytes are reachable from nothing. The report accounts for withheld
objects separately rather than dropping them, so the number is auditable.

`gc` requires unrestricted read authority. A ref-scoped capability sees a
filtered ref list, and a filtered root set is precisely how a collector deletes
live objects.

## Why collection is not implemented

Deleting an object is unrecoverable — objects are the durable substrate — so
the bar is proof, not confidence. Three gaps remain, and none of them is a
matter of adding a flag:

1. **There is no session lease.** `namespaces` records `created_ms` and nothing
   else; a session never expires and never closes. `--min-age-secs` bounds the
   put-before-commit window but it cannot bound how long a session may hold a
   pin it has not yet written. The grace period the issue asks for ("exceeding
   the maximum session lease") has no maximum session lease to exceed. A lease
   column, renewed by session activity, is the prerequisite.
2. **The root read and the deletion are not one transaction.** A root published
   after the walk and before the unlink is invisible to the walk and fatal to
   the object. A collector needs a durable collection epoch that new roots are
   stamped against, so that anything published after the epoch opened is
   off-limits to that collection.
3. **Deleting the bytes is not the whole deletion.** `object_intro` rows point
   at objects, and `fsck --full` roots them; a collector that removes bytes
   without removing the matching catalog rows converts reclaimed space into
   reported corruption. `Store`'s hot LRU object caches also keep serving a
   deleted object inside the collecting process, which hides exactly the bug a
   collector must not have.

Until those three are closed, `gc` reports and refuses. That is the honest
state, and the report is already the useful half: it makes the growth in #309
measurable, and `abandon` makes it bounded.
