# ForgeFS invariants

**Immutable bytes. Explicit authority. Snapshot reasoning. Deterministic integration. Loud conflicts. Verifiable releases.**

```
objects     write-once; ObjectId = BLAKE3(canonical file bytes)
refs        only mutable publication surface; every move is CAS(expected -> new)
session     (cap, ns, pinned_base_oid, observation_set)
checkin     durable objects first; then one atomic metadata publication
conflict    first-class object, never only a string
seal        signed snapshot; tag is frozen; verify rereads durable bytes
cap         (operation, resource); attenuation subset-of parent
boundary    Forge API/socket; direct .forge access is trusted administration
```

| ID | Rule |
|---|---|
| I1 | Decode(encode(x)) is encode(x). Non-canonical bytes are Corrupt. |
| I2 | One logical object => one byte string => one ObjectId. |
| I3 | Put is idempotent iff bytes match; never overwrite. |
| I4 | A committed ref implies fsynced object bytes and parent directory. |
| I5 | Refs move only expected->new. Lost CAS forks or denies. Protected refs deny. |
| I6 | One logical metadata transition commits together: ref/reflog/seal/session cleanup/provenance as applicable. |
| I7 | `tags/`, `conflicts/`, `heads/`, and `forks/` are typed, not naming conventions. |
| I8 | `session.open` pins a base OID. Checkin CASes that OID, never a moving head. |
| I9 | Reads record path->OID. Stale observations fail checkin even on disjoint writes. |
| I10 | Checkin is a contribution (base, tree, agent), not a loose message. |
| I11 | Overlap and unresolved structural ambiguity are Conflict objects. |
| I12 | Merge uses real DAG best merge-bases; multiple best bases are never silently collapsed. |
| I13 | Authority(c+d) is a subset of Authority(c). A holder attenuates without the root secret. |
| I14 | No ambient root. Namespace ID is not a capability. Capability enforcement is at the Forge API/protocol boundary; direct `.forge` access is trusted administration. |
| I15 | `verify`/`fsck` reread durable bytes and anchor sealed releases to this Forge's configured signing key. |

Tests are named after these invariants where practical. A correctness PR should be able to name the invariant it protects.

See [THREAT_MODEL.md](THREAT_MODEL.md) for the explicit security boundary.
