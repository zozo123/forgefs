# Cryptographic dependency and feature review

Status: reviewed against `main` for the v0.4.x line (issue #252). This is a dependency-and-usage review, not a new cryptographic design. ForgeFS deliberately composes standard RustCrypto/Dalek primitives and does not implement a cipher, hash, MAC, signature scheme, PRNG, or curve itself.

## Capability authentication: HMAC-SHA256

Purpose: authenticate the complete FMAC capability and every monotonic caveat in its attenuation chain (`crates/forge-cap`).

- Primitive: workspace requirements `hmac 0.13.0` and `sha2 0.11.0`; the committed release lockfile resolves the same versions, on `digest 0.11.3` and `crypto-common 0.2.2`, implementing HMAC-SHA256.
- Root key: exactly 32 bytes from OS `getrandom`; the workspace requirement is `0.2.15` and the committed lockfile resolves `getrandom 0.2.17`. The key is created during repository initialization and stored in `keys/root.secret` through the repository's secret-file path and permission checks.
- Chain: the first tag is `HMAC(root, FMAC-prefix)`; every caveat replaces the key with the previous 32-byte tag and MACs the length-framed caveat. A holder can therefore attenuate from the tag it possesses without learning the root secret.
- Verification: the final tag is checked with the MAC crate's constant-time `Mac::verify_slice` primitive. Do not convert an expected authentication tag to ordinary bytes and compare it with `==`/`!=`.
- Encoding and HMAC-SHA256 are part of the `fmac1_` compatibility contract. Changing either requires an explicit capability-format decision, not a dependency upgrade disguised as cleanup.
- Enforcement: `token_bytes_are_pinned_across_digest_implementations` in `crates/forge-cap/src/lib.rs` pins the exact `fmac1_` bytes of a root capability and of an attenuated one, and re-verifies both pinned tokens against the root secret. Every other test in that module signs and verifies inside a single build, so without this one a change to the HMAC construction -- or to the digest implementation beneath it -- would round-trip cleanly while invalidating every capability already issued. A deliberate format change must edit the pinned constants, which makes the decision visible in review.
- Digest-stack migrations are byte-checked, not assumed. The `hmac 0.12`/`sha2 0.10` to `hmac 0.13`/`sha2 0.11` move (`digest` 0.10 to 0.11) changed no capability bytes: the same chained input produces `5c0eaa19af80c6e348c0bc30f03ca7215c17b4bdc2393094b6145b6cf585832b` under both dependency sets, so capabilities issued before the upgrade remain valid. The only source change it required was importing `KeyInit`, which now carries `new_from_slice` instead of `Mac`.

The authorization parser is separate from authentication: verification proves the bytes were signed; `Cap::allows` intersects the authenticated caveats and must never widen them (I13/I14).

## Sealed releases: Ed25519

Purpose: sign the immutable `Snapshot` identity used by `forge seal` / `forge verify` (`crates/forge-api/src/integration.rs`).

- Primitive: workspace requirement `ed25519-dalek 3.0.0`; the committed release lockfile resolves `ed25519-dalek 3.0.0`. #400 moved the workspace off the 2.x line, so this is a major-version requirement rather than a compatible in-range resolution of an older one.
- Requested feature surface: crate defaults only. ForgeFS does **not** request Dalek's optional `rand_core` feature and does not call `SigningKey::generate`; it fills a 32-byte seed with OS `getrandom 0.2.17` and calls `SigningKey::from_bytes`. Keeping key generation on that one explicit OS-random path is intentional.
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
2. Enable only features exercised by production code. An unused crypto feature is trusted surface and should be removed through an ordinary Cargo feature-graph review.
3. Use the primitive's constant-time verification API for secret-dependent authentication comparisons.
4. Generate long-term key material from the OS CSPRNG through one auditable path; do not introduce a second RNG stack without a measured need.
5. Treat changes to FMAC authentication, object hashes, seal signing, key sizes, or encoded key/signature fields as compatibility/security design changes, not routine dependency bumps.
6. Keep key-permission, forged-input, wrong-trust-root, durable-byte verification, and pinned capability-token known-answer tests in the release gate. The known-answer test is the only one that fails when a digest-stack change alters the `fmac1_` bytes; deleting it restores the silent-break hazard it exists to close.

## Reviewed production paths

- `crates/forge-cap/src/lib.rs`: HMAC construction, chained attenuation, capability verification.
- `crates/forge-api/src/repository.rs`: OS-random HMAC root and Ed25519 seed creation; secret/public key persistence; local public-key re-derivation.
- `crates/forge-api/src/integration.rs`: snapshot construction, Ed25519 signing, trusted-key and signature verification.
- `Cargo.toml` / `Cargo.lock`: direct crypto requirements and the committed resolved dependency graph; #387 removed the unused direct `rand_core` feature request, #400 moved Ed25519 to `ed25519-dalek 3.0.0`, and #401 moved the MAC stack to `hmac 0.13` / `sha2 0.11` on one `digest 0.11` tree.
- `.github/workflows/security.yml` / `.github/workflows/release.yml`: RustSec, cargo-deny, locked builds, SBOM and release evidence.
