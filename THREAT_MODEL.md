# ForgeFS threat model

ForgeFS is a **concurrency, integrity, and authority substrate for autonomous agents**. Its security boundary is deliberately narrower than a sandbox: capabilities govern clients that use the Forge API/protocol. Code or an OS principal that can directly read or modify `.forge` is trusted administration.

## Security goals

ForgeFS aims to guarantee that:

1. immutable objects are content-addressed, canonical, hash-verified, and never silently overwritten;
2. ref/session state transitions are atomic and crash-consistent;
3. concurrent writers never silently clobber one another — stale writers fork or fail;
4. an agent can act only with the `(operation, resource)` authority granted by its capability;
5. namespace identifiers are not authority and one agent cannot use another agent's namespace through the Forge API;
6. reads are snapshot-consistent and stale observations can invalidate checkin;
7. conflicts are durable first-class objects rather than hidden merge side effects;
8. sealed releases verify against this Forge installation's configured signing key and durable bytes.

## Untrusted

Treat these as adversarial:

- an agent process that receives only the Forge API/socket plus an attenuated capability;
- malformed, replayed, over-broad, expired, or incorrectly attenuated capability tokens;
- malformed objects, protocol frames, paths, refs, requests, and imports;
- concurrent writers, stale sessions, crashes, and abrupt process termination;
- a client attempting to address refs, namespaces, or raw object IDs outside its authority.

ForgeFS checks capability authority at the concrete operation/resource boundary, pins session bases, records observations, validates typed object graphs, and performs logical metadata publication in SQLite transactions.

## Trusted administration

The following are outside the capability boundary and therefore trusted:

- an OS account with direct read/write access to `.forge`;
- code that deliberately opens `forge_store::Store` on the repository path;
- holders of the root HMAC secret, root capability, integrator capability, or Ed25519 signing seed;
- the host kernel, filesystem, and storage device below ForgeFS.

`Forge::store` is private so ordinary `forge-api` callers cannot accidentally bypass authorization. `forge-store` remains a systems-layer crate for trusted administration and tooling; it is **not** a sandbox boundary.

## What capabilities do not do

Capabilities do **not** isolate arbitrary native code that already shares ForgeFS's process, address space, OS credentials, or direct filesystem path. No Rust visibility modifier can turn same-principal filesystem access into a security boundary.

If an agent may execute adversarial native code, isolate that code with an OS/container/VM sandbox and do not expose `.forge` or administrative key files inside it. Give the sandbox only the Forge socket/API and a least-authority capability.

## Recommended local deployment

- Run ForgeFS under a dedicated trusted OS user when agents are adversarial.
- Do not mount `.forge`, `.forge/keys`, `root.cap`, or `integrator.cap` into agent sandboxes.
- Prefer the Unix socket for local agents; it is created mode `0600` and requests still require capabilities.
- Keep `.forge/keys` mode `0700`; ForgeFS creates secret/capability files mode `0600` and rejects loose secret permissions on open.
- Treat HTTP mode as an opt-in loopback transport, not a network security boundary.
- Attenuate capabilities per agent and per operation/resource. Never hand a normal agent the root capability.
- Use separate OS/container/VM identities when direct path isolation matters.

## Trust boundary diagram

```text
 UNTRUSTED AGENT / TOOL
        |
        |  attenuated cap: (operation x resource)
        v
 +-------------------------------+
 | Forge API / Unix socket       |  <-- authorization boundary
 | sessions / reads / checkin    |
 | merge / seal / refs           |
 +-------------------------------+
        |
        v
 +-------------------------------+
 | trusted Forge internals       |
 | immutable CAS + SQLite refs   |
 | signing keys + admin tooling  |
 +-------------------------------+
        |
        v
       .forge                     <-- direct access = trusted administration
```

## Design rule

**Agents may be complex; shared truth must be boring.** The core stores immutable facts, performs small atomic ref transitions, and fails loudly on ambiguity. Scheduling, prompts, model backends, tmux/process control, and sandbox implementation belong outside ForgeFS.
