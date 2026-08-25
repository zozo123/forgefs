# Reclamation: `abandon` and `gc`

Issues #12 and #309. #309 argues that garbage collection and the missing
`abandon` verb are one problem. `abandon` and `gc --dry-run` shipped first;
`gc --collect` now closes it, and the invariant it preserves is **I19**.

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

Reports what a collector would reclaim and deletes nothing. Exactly one of
`--dry-run` and `--collect` is required; a bare `forge gc` still exits 1 with a
diagnostic pointing here, so nothing deletes by default.

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

## `gc --collect`

```
forge gc --collect [--min-age-secs N] [--json]
```

Unlinks the objects the report calls collectable and removes the catalog rows
that named them.

### The race, and why content addressing makes it worse

Finding garbage is the easy half. The hard half is that garbage stops being
garbage while you look at it:

* `gc` computes reachability and decides object X is unreachable;
* concurrently a session checks in and publishes a tree referencing X;
* `gc` unlinks X;
* the ref now points at a tree whose child does not exist. I4 broken, silently,
  and objects are the durable substrate so there is no way back.

The second step is not exotic. I3 says a put whose bytes already exist never
rewrites them, so a writer that legitimately reproduces X's bytes gets X's id
back without touching the file. Content addressing therefore *manufactures*
this race: an object can look like month-old cold garbage and be named by a
checkin a millisecond later. Any collector whose safety argument is "the object
is old" is unsound in ForgeFS specifically, and the age floor `--dry-run`
already shipped was exactly that argument.

### What closes it

Four mechanisms, all load-bearing, and each one is load-bearing because a
test failed without it.

1. **The roots and the unlinks are one catalog transaction.** The sweep runs
   inside `BEGIN IMMEDIATE` on the catalog, which is the *cross-process* SQLite
   write lock `cas_ref`, `set_pin`, `overlay_upsert` and `commit_seal` already
   commit under. No root can be published between the mark and the sweep,
   because no root can be published at all while the sweep runs. That is the
   durable collection epoch gap 2 below asked for, and it needed no new column:
   the catalog already had one write lock and every publication already took
   it.
2. **A deduplicating put refreshes the object's age.** This is the
   content-addressing half. "Old" now means "no writer has written or joined
   these bytes for the whole floor", which is the statement a floor has to
   make, rather than "written long ago", which says nothing about who is
   relying on it. It fails closed: a publisher that cannot refresh the age of
   an object it is about to name refuses the put rather than publishing a name
   over bytes a sweep may delete.
3. **The age that decides is read under the object plane's exclusive lock,
   immediately before the unlink.** The candidate scan runs unlocked and may be
   arbitrarily stale. A deduplicating put takes the same lock shared, so a
   refresh either completes before the sweep reads the age -- and the sweep
   withholds a young object -- or after the sweep finished, in which case the
   object is either untouched or already gone and the put republishes the bytes
   instead of naming absent ones. Without this lock there is a window of a few
   microseconds per candidate; a sweep performs it thousands of times a minute,
   so "small" is not the same as "safe".

4. **The doomed set is closed under "nothing surviving references it".**
   `fsck --full` roots every object *file*, not just the catalog's roots, so it
   walks out of surviving garbage as well as out of live refs. A per-object
   rule therefore produces reported corruption the moment a garbage subgraph
   splits across the age floor or the batch limit -- unlink a contribution,
   leave the garbage commit that names it, and `fsck` reports `OBJECT_READ` on
   an edge no live ref ever used. Under a concurrent load that split is not an
   edge case, it is constant. So age and the batch limit decide only what is
   *offered*; the sweep then walks out of everything that survives and spares
   anything it can reach. That walk is deliberately lenient where the root walk
   is fail-closed: it only ever adds to the spared set, so a missing edge in
   already-broken garbage costs nothing, whereas a missing edge during the root
   walk would cost a live subtree.

   This one was not in the original design. It was found by the concurrent
   soak, twice, after the first three mechanisms were in place and looked
   sufficient.

Ordering supplies the fifth: `objects/` is scanned *before* the first root
read, so an object created after the scan is never a candidate. And the walk
fails closed, as `--dry-run`'s always did -- an unreadable or undecodable
object aborts the sweep with `Corrupt` rather than misreporting its children as
garbage.

The three approaches this design did **not** take, and why:

* *An epoch floor at the oldest live session.* A session may hold a pin for a
  month, and objects older than the oldest session are most of the repository,
  so this collects nothing in the steady state ForgeFS is built for. It also
  answers the wrong question: a pin is a catalog row, therefore a root, so a
  long-lived session was never the hazard.
* *A mark-phase write barrier recording every touched object.* This is
  mechanism 2 in a more expensive form -- a durable row per touched object on
  the hot path instead of a timestamp already stored in the inode.
* *Refusing to collect while any session is live.* Honest, and it was the
  fallback if the above had not worked out. It is also useless here: in a
  repository serving thousands of agent sessions, "no session is live" never
  happens.

### The three gaps the earlier draft named

1. **No session lease.** Still true, and it turns out not to block a *sound*
   collector: a pin, a mount, an overlay entry and an observation are all
   catalog rows, so a session's whole closure is a root for as long as the rows
   exist, however long that is. What a lease would buy is reclamation of
   *stranded* sessions, and `abandon session` is the manual verb for that. The
   lease is still worth having; it is no longer a correctness prerequisite.
2. **Root read and deletion not one transaction.** Closed by mechanism 1.
3. **Bytes are not the whole deletion.** Closed: `object_intro` rows are
   deleted in the sweep's own transaction, in both the `oid` and `commit_oid`
   columns, so `fsck --full` -- which roots both -- stays clean; and every
   unlinked object is evicted from `Store`'s tree and blob LRU caches, so
   absence is observable in the process that caused it rather than only after a
   cold reopen.

### The precondition, stated plainly

A writer that put or deduplicated an object more than `--min-age-secs` ago and
has *still* not reached the transaction naming it is outside all three
mechanisms: its object is on disk, reachable from nothing, and older than the
floor. So collection is sound exactly while **no single put-to-publish interval
exceeds `--min-age-secs`**.

The floor is therefore not decoration, and it is refused below a hard minimum
of 60 seconds rather than quietly honoured. The bound to beat is not how long a
checkin takes but how long it can *block*: the catalog's `busy_timeout` is five
seconds per write transaction, and the concurrent soak measured a single
put-to-publish interval of 5.08s -- one whole timeout -- against a median in
the low milliseconds. A floor at that measured tail would be no floor at all.
The minimum is an order of magnitude above it and the default is a day.

One more thing is not a root and never was: an ObjectId held *outside* the
repository, written down by an operator or passed between tools. `landmark` is
the verb that makes such an id a root, and #249 made it ref-unrestricted for
exactly this reason.

### Cost, and what a sweep blocks

A sweep holds the catalog write lock for the duration of its second root read
and its unlink loop, so writers in every process block on it -- for up to
`busy_timeout`, after which they see `Busy`. It unlinks at most
`GC_COLLECT_BATCH_LIMIT` objects per invocation and reports `batch_limited`
when it stopped early, because without a cap that stall grows with the size of
the garbage heap: the bigger the backlog, the longer the outage.

The cap is not cosmetic, and finding out why cost a measurement. An uncapped
sweep holds both locks for as long as it has objects to unlink, and a publisher
blocked on a lock is a publisher whose put-to-publish interval is growing --
the exact quantity the floor has to bound. A 120-second soak with an uncapped
sweep measured that interval at **52s against a 60s floor**: the safety margin
had all but vanished, and it would have vanished entirely on a bigger
repository. The obvious fix, taking the object-plane lock per candidate instead
of once around the loop, shortens the stall and is **wrong**: the same soak
then produced dangling references, because the exclusion has to cover the
sweep, not each of its steps. Capping the batch is what bounds the stall
without weakening the exclusion. The expensive part, the full
reachability walk, deliberately runs *outside* the lock; the locked pass
re-reads the roots and walks only from roots the unlocked pass had not seen,
which is usually none. A sweep is still stop-the-world for the writer plane
while it unlinks, and that is the honest cost of this design's simplicity. An
incremental collector that never stops the world would need per-object
generation stamps, which is a bigger change than this one.

`fsck --full` running concurrently with a sweep may report a candidate it
enumerated and the sweep then unlinked. That is a race between two
administrative operations, not corruption; run them one at a time.

## Evidence

`gc_collect.rs` pins the mechanisms one at a time, deterministically, including
the one that decides this whole design: an object aged past the floor stops
being collectable the moment a writer reproduces its bytes.

`gc_collect_concurrent.rs` is the test that decides whether any of this is
trustworthy, because a single-threaded "it deleted the right objects" test
cannot see a race. It seeds thousands of pre-aged unreachable objects, then runs
six writers opening sessions, writing payloads drawn from exactly that pool --
so nearly every write is a deduplicating put against an object the collector is
entitled to sweep -- checking in, forking on lost CAS and abandoning, while a
collector sweeps in a loop. It asserts `fsck --full` is clean afterwards, and
again after a cold reopen, because the caches of the process that did the
collecting are not evidence about what is on disk. It also *measures* the
slowest put-to-publish interval of the run and fails if it exceeded the floor,
so the precondition above is reported evidence rather than an assumption.
