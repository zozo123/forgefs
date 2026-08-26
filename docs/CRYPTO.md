# Cryptographic dependency and feature review

Status: reviewed against `main` for the v0.4.x line (issue #252). This is a dependency-and-usage review, not a new cryptographic design. ForgeFS deliberately composes standard RustCrypto/Dalek primitives and does not implement a cipher, hash, MAC, signature scheme, PRNG, or curve itself.

## Capability authentication: HMAC-SHA256

Purpose: authenticate the complete FMAC capability and every monotonic caveat in its attenuation chain (`crates/forge-cap`).

- Primitive: workspace requirements `hmac 0.12.1` and `sha2 0.10.8`; the committed release lockfile resolves `hmac 0.12.1` and `sha2 0.10.9`, implementing HMAC-SHA256.
- Root key: exactly 32 bytes from OS `getrandom`; the workspace requirement is `0.2.15` and the committed lockfile resolves `getrandom 0.2.17`. The key is created during repository initialization and stored in `keys/root.secret` through the repository's secret-file path and permission checks.
- Chain: the first tag is `HMAC(root, FMAC-prefix)`; every caveat replaces the key with the previous 32-byte tag and MACs the length-framed caveat. A holder can therefore attenuate from the tag it possesses without learning the root secret.
- Verification: the final tag is checked with the MAC crate's constant-time `Mac::verify_slice` primitive. Do not convert an expected authentication tag to ordinary bytes and compare it with `==`/`!=`.
- Encoding and HMAC-SHA256 are part of the `fmac1_` compatibility contract. Changing either requires an explicit capability-format decision, not a dependency upgrade disguised as cleanup.

The authorization parser is separate from authentication: verification proves the bytes were signed; `Cap::allows` intersects the authenticated caveats and must never widen them (I13/I14).

## Sealed releases: Ed25519

Purpose: sign the immutable `Snapshot` identity used by `forge seal` / `forge verify` (`crates/forge-api/src/integration.rs`).

- Primitive: workspace requirement `ed25519-dalek 2.1.1`; the committed release lockfile resolves the compatible `ed25519-dalek 2.2.0` release.
- Requested feature surface: crate defaults plus the workspace's explicit `rand_core` feature. ForgeFS itself does **not** call `SigningKey::generate`; it fills a 32-byte seed with OS `getrandom 0.2.17` and calls `SigningKey::from_bytes`. Removing the unused `rand_core` request is tracked separately in #387 so this review does not claim a cleanup that has not landed.
- ForgeFS does not directly request Dalek's `batch`, `digest`, `asm`, `pkcs8`, `pem`, `serde`, `legacy_compatibility`, or `hazmat` features. A future dependency change must re-check the fully unified Cargo feature graph, not infer it from this direct-dependency list alone.
- Secret seed: exactly 32 bytes filled directly by locked `getrandom 0.2.17`, then passed to `SigningKey::from_bytes`. This keeps OS randomness in one explicit repository-initialization path.
- Public trust root: `seal.pub` is derived from the local secret seed. On every open ForgeFS derives the public key again and refuses a catalog `cap_root` that disagrees. A snapshot carrying another key is rejected during verification (I15).
- Signed message: ForgeFS canonical-encodes the unsigned `Snapshot`, computes its BLAKE3 ObjectId, and signs those 32 bytes with ordinary Ed25519. This is an application-level digest followed by Ed25519; it is not Ed25519ph and does not request Dalek's `digest` feature.
- Verification uses `VerifyingKey::verify`; ForgeFS does not call Dalek hazmat, batch-verification, legacy-compatibility, or raw signing APIs.

The snapshot object and signature fields are FORMAT data. A future algorithm migration therefore needs an explicit format/version plan and golden fixtures; there is no algorithm-agility switch in v1.

## Content identity: BLAKE3-256

BLAKE3 names immutable ForgeFS objects and is also the application-level digest signed by the seal path. It is not used as the capability MAC and is not a substitute for signature verification. Trust-boundary reads re-hash durable bytes rather than trusting the cache or catalog (I1/I2/I15).

## Dependency policy

Release CI installs no cryptographic implementation dynamically. `Cargo.lock` is committed, release builds use `--locked`, `cargo audit --deny warnings` gates RustSec, and `cargo deny --locked check advisories bans licenses sources` gates advisories, licenses, bans and dependency sources. Release/security tooling is itself version-pinned.

Review rule for future changes:

1. Prefer the primitive crate's high-level safe API; never add bespoke crypto or raw/hazmat APIs to save glue code.
2. Enable only features exercised by production code. An unused crypto feature is trusted surface and should be removed through an ordinary Cargo-regenerated lockfile change.
3. Use the primitive's constant-time verification API for secret-dependent authentication comparisons.
4. Generate long-term key material from the OS CSPRNG through one auditable path; do not introduce a second RNG stack without a measured need.
5. Treat changes to FMAC authentication, object hashes, seal signing, key sizes, or encoded key/signature fields as compatibility/security design changes, not routine dependency bumps.
6. Keep key-permission, forged-input, wrong-trust-root, and durable-byte verification tests in the release gate.

## Reviewed production paths

- `crates/forge-cap/src/lib.rs`: HMAC construction, chained attenuation, capability verification.
- `crates/forge-api/src/repository.rs`: OS-random HMAC root and Ed25519 seed creation; secret/public key persistence; local public-key re-derivation.
- `crates/forge-api/src/integration.rs`: snapshot construction, Ed25519 signing, trusted-key and signature verification.
- `Cargo.toml` / `Cargo.lock`: direct crypto requirements and the committed resolved dependency graph; #387 owns the unused direct `rand_core` request.
- `.github/workflows/security.yml` / `.github/workflows/release.yml`: RustSec, cargo-deny, locked builds, SBOM and release evidence.
