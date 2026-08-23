# ForgeFS invariants

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

```
objects     write-once; ObjectId = BLAKE3(canonical file bytes)
refs        only mutable surface; every move is CAS(expected → new)
session     (cap, ns, pinned_base_oid, observation_set)
checkin     overlay folded onto the pinned base, then CAS that oid
conflict    first-class object, never only a string
seal        signed snapshot; tag is frozen; verify rereads durable bytes
cap         (operation, resource); attenuation ⊆ parent
```

| ID | Rule |
|---|---|
| I1 | Decode(encode(x)) is encode(x). Non-canonical bytes are Corrupt. |
| I2 | One logical object ⇒ one byte string ⇒ one ObjectId. |
| I3 | Put is idempotent iff bytes match; never overwrite. |
| I4 | A committed ref implies fsynced object bytes and every directory edge needed to reach them; visibility alone is never a durability proof. |
| I5 | Refs move only expected→new. Lost CAS forks or denies. Protected refs deny. |
| I6 | Ref + reflog (+ seal) commit together. |
| I7 | tags/ conflicts/ heads/ are typed, not naming conventions. |
| I8 | session.open pins a base OID. Checkin CASes that oid, never a moving head. |
| I9 | Reads record path→oid. Stale observations fail checkin even on disjoint writes. |
| I10 | Checkin is a contribution (base, tree, agent), not a loose message. |
| I11 | Overlap is a Conflict object. |
| I12 | Merge uses real DAG merge-bases. |
| I13 | Authority(c+d) ⊆ Authority(c). Holder attenuates without the root secret. |
| I14 | No ambient root. Namespace ID is not a capability. |
| I15 | verify/fsck reread durable bytes and this forge's seal key. |
| I16 | Tree names are exact UTF-8 bytes: no Unicode normalization or case folding occurs in core identity. |

A composed Unicode name and its canonically equivalent decomposed spelling are distinct ForgeFS entries if their UTF-8 byte strings differ. Likewise `Foo` and `foo` are distinct. Export adapters must detect target-filesystem collisions and fail rather than silently normalize, fold, or overwrite names.

Tests are named after these IDs. A PR that cannot name an invariant does not merge.
