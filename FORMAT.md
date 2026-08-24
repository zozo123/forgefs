# ForgeFS object format v1

`.forge/VERSION` is the repository codec gate. A newly initialized repository
writes exactly `1\n`, and a reader must accept the VERSION before interpreting
object or catalog bytes.

## Release boundary

Repository VERSION and package release version are different namespaces.
VERSION `1` names the on-disk codec described here; a release tag names a
supported binary build.

The format audit at `2911818` found no local or remote Git tags. Consequently,
all earlier source SHAs are pre-release development builds, not mutually
compatible releases. In particular, adding Contribution (`0x06`) and the
optional `Commit.contrib` field while those builds were untagged finalizes the
pre-release VERSION 1 contract; it does not promise that a pre-Contribution
development binary can read a current repository.

This exception ends at the first release tag. After a VERSION 1 binary is
released, a change that makes that released reader unable to interpret newly
written objects requires a new repository VERSION. Existing object IDs and the
checked-in canonical fixtures are never regenerated or reinterpreted.

| Repository/input | Current VERSION 1 reader | Older untagged development reader | Future reader |
|---|---|---|---|
| VERSION 1 commit without `contrib` | Accept; decode `contrib = None` | Unsupported build-to-build contract | Must preserve bytes and identity |
| VERSION 1 commit with `contrib` and Contribution objects | Accept | Unsupported; may fail closed | Must preserve bytes and identity |
| Unknown/future repository VERSION | Fail before interpretation | No guarantee | Accept only if that reader explicitly supports it |

## Object identity and framing

An `ObjectId` is BLAKE3-256 over the complete encoded object file. There is no
additional hash-domain prefix in VERSION 1. The type byte supplies type-domain
separation because it is the first byte of the hashed file.

Every file is:

```text
type:u8 | header_length:u32-be | header:canonical-CBOR | payload:bytes
```

The header uses ForgeFS's restricted RFC 8949 deterministic CBOR subset:
definite lengths, shortest integer/length encodings, no tags, and map keys
strictly increasing by their encoded CBOR bytes. A typed decoder rejects
unknown keys, missing required keys, trailing header bytes, forbidden payload,
non-canonical encodings, and the wrong type byte.

## VERSION 1 type registry

| Byte | Type | Header / payload summary |
|---|---|---|
| `0x01` | Blob | `{size: uint}` / exactly `size` bytes |
| `0x02` | Tree | `{e: [entry...]}` / empty |
| `0x03` | Commit | required commit map plus optional `contrib` / empty |
| `0x04` | Conflict | canonical conflict map / empty |
| `0x05` | Snapshot | canonical signed-snapshot map / empty |
| `0x06` | Contribution | canonical contribution receipt map / empty |

`0x00` and `0x07` through `0xff` are unassigned in VERSION 1 and fail closed.
Assigning incompatible meaning to an existing byte, changing framing or
canonical encoding in a way that changes identity, or adding an object type
after the first VERSION 1 release requires a new repository VERSION.

## Commit contract

A Commit header has these required fields:

| Key | Value |
|---|---|
| `agent` | UTF-8 text |
| `lm` | boolean landmark flag |
| `msg` | UTF-8 text |
| `parents` | ordered array of 32-byte Commit IDs |
| `tree` | 32-byte Tree ID |
| `ts` | unsigned integer; metadata, not causal order |

It may additionally contain `contrib`, a 32-byte Contribution ID. Omission is
the only canonical encoding of `None`; an explicit CBOR null is invalid. A
present field changes the Commit's bytes and ObjectId. Full graph verification
must follow the edge and require an object of type `0x06`.

`testdata/canonical/commit.hex` freezes the field-absent representation and
`commit_with_contribution.hex` freezes the field-present representation. Both
must decode and re-encode byte-for-byte with their recorded ObjectIds.

## Contribution contract

A Contribution (`0x06`) has an empty payload and the required header map:

```text
{
  ts: uint,
  base: bstr32,
  tree: bstr32,
  agent: text,
  reads: [{p: text, id: bstr32}...],
  writes: [text...],
  parents: [bstr32...]
}
```

| Key | Meaning |
|---|---|
| `base` | pinned Commit whose tree the checkin transformed |
| `tree` | resulting immutable Tree |
| `parents` | ordered Commit inputs to the receipt |
| `reads` | sorted path and observed Blob facts consumed by the checkin |
| `writes` | sorted paths changed by the checkin |
| `agent` | authenticated capability identity that published the checkin |
| `ts` | informational timestamp hint; never causal or a merge key |

Canonical CBOR ordering is ordering by the complete encoded key bytes, not by
the human spelling of keys. The header order is therefore `ts`, `base`, `tree`,
`agent`, `reads`, `writes`, `parents`. Each read-map order is `p`, then `id`
because the one-byte text key encodes before the two-byte key. The canonical
fixture freezes both orders.

`base` and every `parents` item name Commits, `tree` names a Tree, and each
`reads[].id` names a Blob when the graph is fully verified. `reads` and
`writes` paths are non-empty, NUL-free UTF-8 strings of at most 4096 bytes and
are strictly increasing and unique by raw UTF-8 bytes. Limits are 1024 parents,
100,000 reads, 100,000 writes, 1024 agent bytes, and an 8 MiB encoded header.

### Contribution join law

A Contribution ObjectId is an immutable monotonic fact. Let `C(c)` be the set
of Contribution OIDs in the typed graph reachable from Commit `c`. A merge
commit has both input commits as parents and does not copy, replace, rank, or
retract their receipts, so:

```text
C(merge(a, b)) = C(a) union C(b)
```

Set union makes receipt reachability commutative, associative, and idempotent,
even though parent order still participates in the merge Commit's own identity.
Adding a descendant can only preserve or grow its reachable Contribution set.
Mutable “current contribution” pointers, decrementing counters, last-writer
wins, and wall-clock maxima as causal truth are outside the object contract.

## Snapshot provenance manifest

`Snapshot.prov` names a Blob whose current payload is the canonical CBOR map
`{entries: {oid_hex: attribution}, version: 1}`. `oid_hex` is lowercase
64-character ObjectId hex; an attribution is UTF-8 text of at most 1024 bytes.
The entry map has at most 1,000,000 items and the complete canonical payload is
at most 64 MiB. Its exact key set is the union of:

- every Tree and Blob reachable from `Snapshot.tree`; and
- every Contribution reachable from `Snapshot.commit` through the typed
  immutable graph.

Tree/Blob labels preserve the v1 first-introducer hint (or `unknown`). A
Contribution label must equal that receipt's immutable `agent` field. Missing
or additional keys, a mismatched Contribution agent, malformed/non-canonical
CBOR, and a wrongly typed graph edge are corruption. The snapshot signature
binds the manifest because `prov` is the Blob's ObjectId. This payload rule
formalizes the existing VERSION 1 Snapshot field; it does not change any
object framing or type encoding.

Earlier ForgeFS builds wrote the entry map directly as the Blob payload and
included only the sealed Tree/Blob closure. Readers identify that
legacy shape unambiguously because its top-level keys are 64-character OIDs;
they preserve its content-only verification semantics while still walking and
type-checking the reachable Contribution graph. New seals always write the
versioned envelope and must attest Contributions. Unknown envelope versions or
fields fail closed.

## Metadata schema is a separate contract

`meta.sqlite` is mutable and is not part of an ObjectId. Its
`schema_migrations` ledger is independent of `.forge/VERSION`; a metadata
migration may change SQLite state but may never rewrite immutable object files
or change their hashes.

The current metadata schema is version 2. Creation and upgrade are separate
paths: `0 -> 2` creates the current schema and stamps the whole ledger
atomically, while an existing catalog is stepped forward one version at a time
in a single immediate transaction, so an interruption leaves either the old or
the new schema complete. Version 2 widens `observations` so a recorded read can
say it saw a directory or saw nothing, and carries every v1 row forward as the
blob read it was. A version above `CURRENT_SCHEMA_VERSION` stays a fail-closed
error, and a read-only open still refuses to migrate. None of this requires
changing the repository VERSION: no immutable object file or ObjectId moves.
