# ForgeFS object format v1

`.forge/VERSION` is the repository codec version. A newly initialized ForgeFS repository writes exactly `1\n`.

For VERSION 1:

- object files are immutable and content-addressed;
- an `ObjectId` is BLAKE3-256 over the complete encoded object file bytes;
- the encoded object framing/type byte is therefore part of the hash input;
- current object type bytes are `0x01` through `0x05`;
- readers fail closed on unknown object types/versions rather than reinterpreting bytes;
- old immutable objects are never rewritten as part of a metadata/schema migration.

There is no additional hash-domain prefix in v1. Introducing a different hash domain, changing the canonical object encoding in a way that changes identity, or assigning incompatible meaning to an existing type byte requires a new repository VERSION. Existing v1 object IDs remain v1 identities permanently.

Mutable SQLite metadata schema versioning is separate from this immutable object-format version.
