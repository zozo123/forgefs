# Releasing ForgeFS

ForgeFS has one release authority path: `.github/workflows/release.yml`.

The model is intentionally small:

1. `Cargo.toml` `[workspace.package].version` is the single version source.
2. `Cargo.lock` is committed and must match that versioned workspace.
3. A release-preparation workflow opens the version-bump PR.
4. The release tag must exactly equal `v<workspace version>` and its commit must already be reachable from `origin/main`.
5. Four target artifacts are built from that exact commit.
6. Three native target artifacts run the end-to-end ForgeFS release gate from the packaged binary, not from a rebuild.
7. All binaries, BUILD-INFO files and gate evidence are assembled into one `release-payload` artifact.
8. `SHA256SUMS` covers every file in that payload.
9. The exact checksum manifest is attested.
10. The publish job downloads only that immutable payload, re-verifies it, and hands exactly those files to `gh release create`.

No other workflow may create ForgeFS releases.

## Prepare the next version

Use **Actions -> prepare release -> Run workflow** and choose `patch`, `minor`, or `major`.

The workflow:

- reads the current workspace version;
- computes the next stable SemVer;
- edits the single workspace version field;
- runs `cargo update -w` so every local workspace package entry in `Cargo.lock` matches;
- proves `scripts/verify-tag-version.sh vX.Y.Z`;
- runs fmt, check, clippy, tests, the CLI ABI table and the end-to-end release gate;
- pushes `release/vX.Y.Z` and opens a release-preparation PR.

The generated PR contains only the version/lockfile transition. Review and merge it normally.

## Publish

After the release-preparation PR is merged, tag the exact merged commit:

```bash
git switch main
git pull --ff-only
git tag -a vX.Y.Z -m 'ForgeFS vX.Y.Z'
git push origin vX.Y.Z
```

That tag triggers `.github/workflows/release.yml`.

The workflow refuses publication unless all of the following are true:

- the tag is canonical `vMAJOR.MINOR.PATCH[-PRERELEASE]`;
- `scripts/verify-tag-version.sh` says the tag equals the workspace version;
- every workspace member inherits the workspace version;
- the tagged commit is already an ancestor of `origin/main`;
- `cargo fmt`, `check`, `clippy -D warnings`, and all workspace tests pass on Linux and macOS;
- Rust 1.89 MSRV check/tests pass;
- `cargo audit` and `cargo deny` pass against the committed lockfile;
- all four release targets build;
- every natively runnable packaged binary reports the expected version;
- the three native target packages pass `scripts/release-gate.sh`;
- the assembled payload contains four tarballs and the expected gate/fsck evidence;
- every payload file verifies against `SHA256SUMS`;
- provenance attestation succeeds;
- the `release` GitHub Environment approves the publishing jobs.

Only then does `publish` receive `contents: write`.

## Rehearse without publishing

`release.yml` also runs on relevant pull requests and `workflow_dispatch`.

Those runs execute identity validation, correctness gates, builds, packaged-binary E2E checks and payload assembly, but they do **not** attest or publish because there is no release tag. This is important: the packaging/payload path is exercised before a real release rather than being a production-only branch.

## Version rules

Do not duplicate a release version in workflow YAML, release notes, or scripts. Artifact names and release notes receive the version from the `identity` job, which reads the workspace version.

It is valid for normal development on `main` to retain the most recently released version. The version changes in an explicit release-preparation PR. This keeps version bumps reviewable and avoids meaningless post-release commits whose only purpose is to guess the next release number.

For prereleases, prepare the exact prerelease version manually in `Cargo.toml`/`Cargo.lock` and verify it with `scripts/verify-tag-version.sh vX.Y.Z-rc.N` before tagging. The automated prepare workflow intentionally produces stable major/minor/patch versions only.

## Release payload

`release-payload` is the only cross-job handoff consumed by the publisher. GitHub Actions job filesystems are not shared; therefore the workflow never assumes files downloaded in `assemble` or `attest` magically exist in `publish`.

The payload contains:

- `forge-<version>-<target>.tar.gz` for all four supported targets;
- one `BUILD-INFO` per target;
- native release-gate evidence such as gate summary, full fsck, CLI ABI, seal attestation and environment line;
- `SHA256SUMS` covering every regular payload file except itself.

Intermediate checksum sidecars are not published. `SHA256SUMS` is the single checksum manifest.

## Supported targets

| Target | Build | Packaged-binary E2E |
|---|---|---|
| `x86_64-unknown-linux-gnu` | native Ubuntu | yes |
| `aarch64-unknown-linux-gnu` | native Ubuntu arm64 | yes |
| `x86_64-apple-darwin` | cross-build on macOS arm64 | no Rosetta assumption |
| `aarch64-apple-darwin` | native macOS | yes |

The x86_64 macOS binary is still architecture-checked during build and included in the attested checksum manifest. It is not executed because hosted arm64 macOS runners are not guaranteed to provide Rosetta.

## Repository settings that are part of the release contract

Configure a GitHub Environment named `release` with required reviewers and restrict deployment to release tags (`v*`). Both attestation and publication use this environment.

Protect `main` with required status checks. Code can encode the gates, but repository settings are the authority that prevents bypassing them.

Keep workflow actions SHA-pinned on trusted/release paths. A movable action tag is code execution from a mutable dependency.

## Failure policy

A release is fail-closed:

- an existing GitHub release with the same tag is never mutated automatically;
- a non-main tag is rejected before expensive build work;
- a version mismatch is rejected before build work;
- missing evidence prevents payload assembly;
- a checksum mismatch prevents attestation and publication;
- a failed gate never produces a release.

Fix the source or workflow and cut a new release attempt deliberately. Do not weaken the gate to make a tag green.
