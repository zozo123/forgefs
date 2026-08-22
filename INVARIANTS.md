# ForgeFS invariants

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

```
objects     write-once; ObjectId = BLAKE3(canonical file bytes)
refs        only mutable publication surface; moves are expected → new
session     (cap, namespace, pinned_base_oid, observation_set, live_ref)
checkin     overlay folded onto pinned base; publication + session transition atomic
conflict    first-class object, including ambiguous/multiple merge bases
seal        signed snapshot; tag frozen; verify rereads durable typed closure
cap         (operation, resource); attenuation ⊆ parent
```

| ID | Rule |
|---|---|
| I1 | Decode(encode(x)) is encode(x). Non-canonical bytes are `Corrupt`. |
| I2 | One logical object ⇒ one byte string ⇒ one ObjectId. |
| I3 | Put is idempotent iff bytes match; never overwrite. |
| I4 | A committed ref implies fsynced object bytes and parent directory. |
| I5 | Refs move only expected→new. Lost CAS forks or denies. Protected refs deny. |
| I6 | One logical metadata transition is one SQLite transaction: ref+reflog, seal metadata, session creation, or checkin/fork+pin+mount+overlay+observations. |
| I7 | `main`, `heads/`, `forks/`, `conflicts/`, and `tags/` enforce target kind at the storage boundary; generic mutation cannot forge a sealed tag. |
| I8 | `session open` pins a base OID. Checkin CASes that OID, never a moving head. |
| I9 | Reads record path→OID. Stale observations fail checkin even on disjoint writes. |
| I10 | First-introducer provenance is batched atomically before ref publication; immutable contribution receipts may supersede the legacy table. |
| I11 | Semantic overlap is a `Conflict` object. Corruption/type confusion is never downgraded to conflict. |
| I12 | Merge computes all best DAG merge bases. Multiple best bases become an explicit conflict, never an arbitrary traversal choice. |
| I13 | Authority(c+d) ⊆ Authority(c). Holder attenuation never needs the root secret; the root HMAC minting secret never lives in mutable SQLite metadata. |
| I14 | No ambient root. Namespace ID is not authority. `Forge` does not expose raw `Store` access across the capability boundary. |
| I15 | `verify`/`fsck` bypass hot caches, rehash durable bytes, type-check snapshot→commit→tree/provenance closure, and anchor signatures to this forge's trusted seal key. |

A correctness PR names the invariant it defends. A performance PR names the workload it wins without weakening an invariant.
