# Conflicts: detection, resolution, and merge drivers

Owns the conflict lifecycle contract and the decisions on issues #15 and #16.
`INVARIANTS.md` owns I11 and I12 themselves; this document owns what is built on
top of them, what is deliberately absent, and what it would cost to add.

Status: **detection is complete; PR1 local resolution ships; sealed
object-plane binding still needs FORMAT VERSION 2.** `forge resolve` advances
`--into`, publishes `resolutions/<conflict-ulid>`, and retires the conflict
ref. `merge --resolved` stays refused. A durable conflict OID inside Commit or
Contribution provenance remains a VERSION 2 change (section 4); #361 closed
that bump as not planned for v1.

## 1. What exists today

A merge that cannot be decided by the deterministic three-way rule publishes a
`Conflict` object and a typed ref naming it, then fails with exit 4. Nothing is
merged and no ref moves.

| Piece | Where |
|---|---|
| `Conflict { bases, ours, theirs, paths, causal }` | `crates/forge-core/src/object.rs` |
| `ConflictPath { path, a, b, base }` | same |
| conflict construction, both `MergeOutcome::Conflict` and multiple-merge-base | `crates/forge-api/src/integration.rs`, `Forge::merge` |
| the deterministic merge itself | `crates/forge-merge/src/lib.rs`, `three_way` / `merge_trees` |
| the typed ref `conflicts/<into>/<ULID>`, kind `conflict` | `Forge::merge` |
| rendering | `Forge::show`, `crates/forge-api/tests/show_conflict.rs` |
| bounds: at most `MAX_CONFLICT_ITEMS` = 100_000 paths, bases, or causal ids | `Conflict::validate` |

Observed end to end on `main@4593afc`, two agents writing the same path:

```text
$ forge merge --into=main --from heads/agents/anon/<B>
conflict e46e8128...
exit=4

$ forge show oid:e46e8128...
conflict e46e8128f9ad40d1c532e14db9ef367f55023db724202a83b05a94e08bb29379
bases 0bc7d9c63a16d8dd7872628dea97402a93cbd85b86ca31036faf8ddeb5e5fcfb
ours 2062fdd4bdfb41690e0cc047fa89de53214ee8c152a1f0ec2b3c8a77b4d871b2
theirs a73a7ca465cb0838152d39444b906dea9553c6c7e583d96b7e1bd7f4ffbead36
path same.txt a=3f60eef6... b=1cdf1348... base=-
causal 47a34066...,774d48e6...
```

Both sides survive as immutable, reachable objects. The merge is direction
symmetric (`property_merge_symmetry.rs`), causality comes only from the parent
DAG (`clock_causality.rs`, `merge_bases.rs`), and a merge over more than
`MAX_CONFLICT_ITEMS` paths is refused as `Invalid` rather than writing an object
its own decoder would call `Corrupt` (#355).

So most of what #15 asks for on the detection side is already true. What follows
is the rest.

## 2. Gap A: raw `--resolved` stays refused; `forge resolve` is the verb

`Forge::merge` takes a `resolved: Option<ObjectId>` argument and rejects every
value of it:

```rust
if resolved.is_some() {
    return Err(Error::Invalid(RAW_MERGE_RESOLUTION_DISABLED.into()));
}
```

`RAW_MERGE_RESOLUTION_DISABLED` is a stable string --
`"raw merge resolution is disabled; resolution must be bound to a conflict object"`
-- asserted by `crates/forge-api/tests/merge_resolution_safety.rs` and by
`crates/forge-cli/tests/e2e.rs`. The flag survives at the parser boundary for
compatibility only.

Observed: `--resolved` is refused for a tree OID **and** for the conflict OID
itself, both exit 1. That fail-closed posture stays: a raw tree on `merge` is
still not a resolution.

`forge resolve --conflict <oid> --into <ref> (--ours | --theirs | --tree <oid>)`
is the consuming verb. It checks that `<oid>` is a published Conflict under
`conflicts/<into>/`, that `--into` still holds one of the conflict's causal
commits, and that a `--tree` differs from the auto-merge only on paths the
conflict names. It publishes a merge commit whose parents are the causal tips,
writes `resolutions/<conflict-ulid>` at that commit, and retires the conflict
ref in the same transaction. A moved head is exit 4 (`StaleObservation`), never
a fork. Tests live in `crates/forge-api/tests/resolve_conflict.rs`.

### Gap A2: conflict refs are retired by resolve

`conflicts/<into>/<ULID>` remains a GC root while the conflict is open.
`forge resolve` retires it (same reflog abandon shape as `abandon fork`, but
only for `conflicts/` names and only as part of a successful resolve).
`forge abandon` still takes only `fork` and `session`; conflict retirement is
not a free-standing abandon verb.

## 3. Gap B: `ConflictPath` cannot say what differs

`ConflictPath` carries a path and three optional ObjectIds. It records no entry
kind and no executable bit, although `forge-merge` treats entry identity as
`(id, kind, exec)` and will raise a conflict on a mode-only change.

Observed, one agent writing `/x` as a file and another writing `/x/y`:

```text
path x a=84010ce3... b=5dbdfbb2... base=-

$ forge show oid:84010ce3...   ->  blob 22 bytes
$ forge show oid:5dbdfbb2...   ->  tree 57 bytes
```

A file-versus-directory conflict is rendered as two indistinguishable hexes. The
consumer must issue one extra `show` per side to learn the kinds, and for an
exec-bit-only conflict `a` and `b` would be the *same* ObjectId with nothing at
all to distinguish them.

#15 asks the conflict object to record "entry kind/mode changes, delete-vs-modify,
rename candidates". Delete-vs-modify is representable (`a` or `b` is absent).
Kind and mode are not.

Exact rename *detection* does not require new fields. The merge compares the
three immutable trees and recognizes only a one-to-one relocation of a unique
full entry identity `(oid, kind, exec)`. Divergent exact destinations and exact
rename-versus-delete now publish a conflict at the removed source
(`a=- b=- base=<oid>`); matching destinations converge. Equal subtrees are
skipped by ObjectId, and same-name directory rewrites are descended, so files
moved across existing directories are covered. Duplicate source identities,
multiple matching destinations, modified moves, and content-similarity guesses
remain deliberately ambiguous rather than manufacturing trusted-core intent.
`rename_characterisation.rs` pins both the detected cases and the rule that a
copy whose source remains is not inferred as a move (#39).

What VERSION 1 still cannot do is *label* the conflict as rename/rename versus
rename/delete, or encode kind and mode in `ConflictPath`. Adding those keys is
the same VERSION problem as section 4.

## 4. The decision: the binding requires FORMAT VERSION 2

#15's load-bearing requirement is that the **resolution commit reference the
resolved conflict OID in provenance data**, not in a log message, and that
`fsck`/`verify` be able to walk that relationship. Take that seriously and it
lands on the frozen format.

`FORMAT.md` freezes VERSION 1, `v0.3.0` is tagged, and the pre-release exception
has closed. I1 makes unknown header keys `Corrupt`, and FORMAT.md states it
normatively: a typed decoder rejects unknown keys. So:

Since #300, `FORMAT.md` also enumerates the `Contribution` header keys
explicitly -- `ts`, `base`, `tree`, `agent`, `reads`, `writes`, `parents` -- and
states that the canonical fixture freezes that key set and its encoded order. So
adding a key there is not merely an unknown key to a v1 decoder; it moves a
frozen fixture.

| Way to bind a resolution to its conflict | Cost |
|---|---|
| add a `conflict` key to `Contribution` (`base, tree, parents, reads, writes, agent, ts`) | unknown key to a released v1 reader -- **VERSION 2** |
| add a `resolves` key to `Commit` | same -- **VERSION 2** |
| a new `Resolution` object type | unassigned type bytes fail closed -- **VERSION 2** |
| put the conflict OID in `Commit.msg` | exactly the "rely on a log message" #15 forbids |
| put it in `Contribution.parents` | type confusion; the typed graph walk expects commits |
| a typed ref `resolutions/<...>` in the catalog | **no format change**, see below |

The ref option is the only v1-compatible one, and its limit must be stated
rather than discovered: refs are the mutable catalog plane. A seal manifest
binds objects, so a ref-borne binding is **not** sealed provenance, does not
survive `verify` of a sealed snapshot on another host, and does not replicate
under the object/ref split in `docs/REPLICATION.md`. It buys local auditability
and a working verb; it does not buy I15-grade provenance.

### Consequence for #11

`docs/CHUNKING.md` trigger condition 3 says the VERSION bump is the dominant
cost of chunking and "should be spent once. If another accepted change already
requires VERSION 2, chunking should be re-evaluated as part of that change."

Conflict-bound resolution is that other change. **#11 and #15 should be
evaluated together as one FORMAT v2 epic, or neither should bump.** Adding a
`ConflictPath` kind/exec key (section 3) rides along at near-zero marginal cost
once the gate is open, and so does the `File` object type.

That is the decision this document records. It was not visible from either issue
alone.

## 5. Shipped shape (PR1) and the VERSION 2 remainder

**PR 1 (shipped, no format change).** See section 2. The scope check is what
makes "resolve" different from "merge with a tree I made up", and it is
checkable in v1 because it compares trees, not provenance. The
`resolutions/<ulid>` ref is catalog state: it is not sealed provenance and does
not survive `verify` of a sealed snapshot on another host.

**PR 2 (VERSION 2, not planned for v1 with #361).** Move the binding into the
object plane, add `kind`/`exec` to `ConflictPath`, and extend the seal manifest
walk so `verify` proves the resolution named its conflict. Test 5 from #15
(identical inputs produce identical bytes) still requires any resolution record
to exclude wall-clock fields -- `Commit.ts` and `Contribution.ts` are both
present today.

## 6. Semantic merge drivers (#16)

#16 is blocked by #15, and the block is not administrative. A driver contract is
`(base, ours, theirs) -> deterministic bytes + evidence`, with fallback to a
normal conflict object on failure or timeout, and sealed provenance recording
driver name, version and hash. Every one of those clauses is a statement about
the resolution record. There is no resolution record, so the driver interface
cannot be specified, only guessed at.

Recording driver name, version and hash durably is the same unknown-key problem
as section 4, so #16 inherits VERSION 2 as well.

#119 and #139 both list #16 under **Park**. That remains correct. The research
gate in #16 -- benchmark real multi-agent traces, ship only drivers that reduce
conflicts without raising the silent-mismerge rate -- also has no corpus behind
it; nobody has measured the conflict rate or the machine-resolvable fraction on
a real swarm trace. That measurement is cheap, independent of everything above,
and needs no format change. It is the honest first step for #16.

## 7. Scale

PR 1 shipped as the local catalog binding above. PR 2 remains a repository
VERSION transition -- the `.forge/VERSION` gate, every decoder, export, mount,
merge, diff, canonical fixtures, GC reachability, `verify`, and the sealed
release path, where a v1 binary cannot verify a v2 snapshot at all. With #361
closed as not planned for v1, that work stays post-v1.
