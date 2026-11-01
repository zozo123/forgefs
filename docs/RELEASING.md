# Releasing ForgeFS

ForgeFS has one release authority path: `.github/workflows/release.yml`.

The model is intentionally small:

1. `Cargo.toml` `[workspace.package].version` is the single version source.
2. `Cargo.lock` is committed and must match that versioned workspace.
3. A release-preparation workflow opens the version-bump PR.
4. The release tag must exactly equal `v<workspace version>` and its commit must already be reachable from `origin/main`.
5. Four target artifacts are built from that exact commit.
6. Each target emits a CycloneDX SBOM for its own resolved dependency graph, cross-checked against the crates the shipped binary demonstrably links.
7. All four target artifacts run the end-to-end ForgeFS release gate on their native hosted architecture, from the packaged binary rather than a rebuild.
8. `x86_64-unknown-linux-gnu` is built and packaged twice, in two clean trees at different paths, and the bytes must match.
9. Release build, gate and audit jobs start without a shared compilation cache.
10. All binaries, BUILD-INFO files, SBOMs, reproducibility evidence and gate evidence are assembled into one `release-payload` artifact.
11. `SHA256SUMS` covers every file in that payload.
12. The exact checksum manifest is attested.
13. The publish job downloads only that immutable payload, re-verifies it, and hands exactly those files to `gh release create`.

No other workflow may create ForgeFS releases.

`docs/SUPPLY-CHAIN.md` records the SBOM tool choice, the reproducibility
result and its exact scope, and the dependency policy.

## Prepare the next version

Use **Actions -> prepare release -> Run workflow** and choose `patch`, `minor`, or `major`.

The workflow:

- performs all version mutation, compilation, tests and release gating in a read-only job;
- reads the current workspace version;
- computes the next stable SemVer;
- edits the single workspace version field;
- runs Cargo once without `--locked` so the existing lockfile is minimally refreshed for the local workspace-version change; it deliberately does not run `cargo update` and therefore does not opt into unrelated dependency upgrades;
- proves `scripts/verify-tag-version.sh vX.Y.Z`;
- runs fmt, locked check, clippy, tests, the CLI ABI table and the end-to-end release gate;
- packages exactly the gated `Cargo.toml`/`Cargo.lock` patch as an immutable workflow artifact;
- gives write permission only to a fresh job that revalidates and applies that exact patch, pushes `release/vX.Y.Z`, and opens the release-preparation PR.

The generated PR contains only the version/lockfile transition. Review and merge it normally.

## Publish

After the release-preparation PR is merged and `main`'s `ci` run for that
exact commit is green (including `admission`), tag that commit:

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
- every context `protect-main` requires has a latest GitHub Actions check run on the tagged commit that concluded `success` (mainline admission evidence, R1);
- `cargo fmt`, `check`, `clippy -D warnings`, and all workspace tests pass on Linux and macOS;
- Rust 1.89 MSRV check/tests pass;
- `cargo audit` and `cargo deny` pass against the committed lockfile;
- all four release targets build;
- no release gate, audit or artifact build restores a shared Cargo/target cache;
- every natively runnable packaged binary reports the expected version;
- all four native target packages pass `scripts/release-gate.sh`;
- every target SBOM lists every crate recovered from that target's shipped binary;
- two clean builds of the tagged commit produce byte-identical packaged files for `x86_64-unknown-linux-gnu`;
- the 41 non-manifest payload files exactly match the audited asset/evidence catalog, with no extra or non-regular entries;
- `SHA256SUMS` covers that exact catalog and every payload file verifies against it;
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
- one CycloneDX SBOM per target, `forge-<version>-<target>.cdx.json`;
- `forge-<version>-x86_64-unknown-linux-gnu.REPRODUCE.txt`, the double-build evidence;
- release-gate evidence for every target: gate summary, full fsck, CLI ABI, seal attestation, conflict object and environment lines;
- `SHA256SUMS` covering every regular payload file except itself.

Intermediate checksum sidecars are not published. `SHA256SUMS` is the single checksum manifest.

## Supported targets

| Target | Build | Packaged-binary E2E |
|---|---|---|
| `x86_64-unknown-linux-gnu` | native Ubuntu | yes |
| `aarch64-unknown-linux-gnu` | native Ubuntu arm64 | yes |
| `x86_64-apple-darwin` | native macOS Intel | yes |
| `aarch64-apple-darwin` | native macOS | yes |

The workflow uses the fixed `macos-15-intel` and `macos-15` labels, so neither macOS artifact depends on cross-execution or Rosetta. Linux likewise uses fixed x86_64 and arm64 Ubuntu 24.04 runners.

## Repository settings that are part of the release contract

Configure a GitHub Environment named `release` with required reviewers and restrict deployment to release tags (`v*`). Both attestation and publication use this environment.

### Mainline admission (R1, issue #51)

**Invariant R1.** No ordinary actor can make a commit the published `main`
head, or publish a release, unless the executable correctness contract
succeeded for that exact commit.

The binding control is the `protect-main` repository ruleset. Its reviewed
source is [`.github/rulesets/protect-main.json`](../.github/rulesets/protect-main.json);
the live ruleset must equal it. It keeps deletion and non-fast-forward
protection, requires a pull request, has no bypass actors, and requires these
**authoritative checks**, each pinned to the GitHub Actions app
(`integration_id` 15368) and required to be up to date with `main`:

| Required context | Job in `.github/workflows/ci.yml` |
|---|---|
| `rust` | fmt, check, clippy, tests on the release toolchain |
| `msrv` | check and tests on Rust 1.89 |
| `macOS durability (macos-15)` | `macos-durability`, Apple silicon `F_FULLFSYNC` path |
| `macOS durability (macos-15-intel)` | `macos-durability`, Intel `F_FULLFSYNC` path |
| `e2e-smoke` | 32-agent concurrent smoke benchmark |
| `power-loss durability (I4)` | `power-loss` page-cache-drop gate |
| `gates` | CLI ABI conformance and the end-to-end release gate |
| `admission` | aggregate: exactly the jobs above, all `success` |

Every gate is required by name, not only through `admission`. `admission`
lives in PR-controlled YAML, so a pull request could rewrite it to depend on
nothing and pass. The per-job contexts close that hole: a pull request that
renames, removes or skips a gate leaves a required context unreported, and
GitHub will not merge it, whatever its replacement reports.

**Trust root.** `.github/` (workflows, admission and release scripts, the
ruleset files and `CODEOWNERS`) is owned by `@zozo123` in
[`.github/CODEOWNERS`](../.github/CODEOWNERS), and the ruleset requires
code-owner review. GitHub reads `CODEOWNERS` from the base branch, so a pull
request cannot remove its own owner. A trust-root change from anyone else
(Dependabot action bumps included) needs the owner's approval. GitHub does not
let an author approve their own pull request, so the owner's own trust-root
changes go through break-glass (below). That is the explicit, auditable path
the invariant asks for, and it applies only to `.github/`. Ordinary source
changes need no review, only green checks.

**Drift lint.** `workflow-security` runs
`.github/scripts/check-admission-contract.py`. It fails when `ci.yml`, the
`REQUIRED` set in `check-ci-admission.py`, the ruleset files and `CODEOWNERS`
disagree: a renamed or conditional gate, an `admission` job with a different
dependency set, a context the ruleset does not require (or requires but CI
never reports), a check not pinned to GitHub Actions, a bypass actor, or a
missing rule. This lint is defense-in-depth only. The live ruleset is the
control.

**Release tags.** [`.github/rulesets/release-tags.json`](../.github/rulesets/release-tags.json)
makes `v*` tags immutable: no update, no deletion, no bypass. A failed release
attempt is fixed by cutting a new version, never by moving a tag. The tag
workflow's `identity` job runs `.github/scripts/release-require-admission.py`,
which reads the required contexts from `protect-main.json` and refuses the
release unless the latest GitHub Actions check run of every one of them on the
tagged commit concluded `success`. A commit that reached `main` without a CI
run cannot be released. Tag only a `main` commit whose `ci` run is green. If
`main`'s head never ran CI, run `gh workflow run ci.yml --ref main`, wait for
`admission`, then re-run the release. This check comes before release.yml's
own gates, which still run in full on the tagged commit.

#### Apply and verify the rulesets

Changing the rulesets is an administrator action. CI never does it.

```bash
# protect-main already exists (id 21234278); replace it with the reviewed file.
gh api -X PUT repos/zozo123/forgefs/rulesets/21234278 \
  --input .github/rulesets/protect-main.json
# The release-tag ruleset is new.
gh api -X POST repos/zozo123/forgefs/rulesets \
  --input .github/rulesets/release-tags.json

# Evidence: the rules GitHub enforces on main right now.
gh api repos/zozo123/forgefs/rules/branches/main \
  --jq '.[] | [.type, (.parameters.required_status_checks // [] | map(.context) | join(", "))] | @tsv'
```

Acceptance checks, run once after applying:

1. A pull request with a deliberately failing test cannot merge.
2. A pull request that renames `gates` (or deletes it from `admission.needs`)
   cannot merge, even when its renamed job passes.
3. `git push origin <commit>:main` without a pull request is rejected.
4. A `v*` tag on a commit lacking green admission fails `release identity`.

#### Break-glass

Break-glass is the only way around R1, and it is never part of normal work.

1. Open an issue that records the reason, the exact commit, which gates failed
   or were unavailable, and who is acting.
2. Disable the ruleset for the shortest possible window, and log it in that
   issue:

   ```bash
   gh api -X PUT repos/zozo123/forgefs/rulesets/21234278 -f enforcement=disabled
   # ... the single merge or push the issue describes ...
   gh api -X PUT repos/zozo123/forgefs/rulesets/21234278 -f enforcement=active
   ```

   Rule-suite insights and the ruleset history record both changes.
3. Re-apply `.github/rulesets/protect-main.json` and confirm with the
   `rules/branches/main` query above.
4. Run every gate on the resulting `main` commit
   (`gh workflow run ci.yml --ref main`) and attach the evidence to the issue.

A bypass never replaces the release workflow's green evidence. It never
authorizes a release over a failed gate, and the release path rejects a commit
without green admission evidence whatever happened on `main`.

Scheduled and dependency-change security runs execute both RustSec audit and
`cargo deny` (advisories, bans, licenses, and sources), including changes to
`deny.toml`. Release jobs enforce the same policy independently.

Keep workflow actions SHA-pinned on trusted/release paths. A movable action tag is code execution from a mutable dependency.

## Failure policy

A release is fail-closed:

- an existing GitHub release with the same tag is never mutated automatically;
- a non-main tag is rejected before expensive build work;
- a version mismatch is rejected before build work;
- missing evidence prevents payload assembly;
- an SBOM that omits a crate the binary links fails its build job;
- a packaging difference between two builds of the same commit fails the release;
- a checksum mismatch prevents attestation and publication;
- a failed gate never produces a release.

Fix the source or workflow and cut a new release attempt deliberately. Do not weaken the gate to make a tag green.
