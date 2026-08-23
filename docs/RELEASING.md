# Releasing ForgeFS

ForgeFS releases are **gated by ForgeFS verifying itself**. A tag does not
produce a release because CI is green; it produces a release because the exact
binary that will be published initialised a forge, ran the two-agent path,
produced a real Conflict object, refused a stale observation, sealed a snapshot,
verified that snapshot from durable bytes, and passed `fsck --full`.

Everything below is executable. Nothing in this document is a claim you cannot
reproduce with one command.

## Cutting a release

```bash
# 1. Decide the version and set it in ONE place.
#    Every workspace member inherits workspace.package.version.
$EDITOR Cargo.toml            # [workspace.package] version = "0.2.0"
cargo check --workspace --all-targets --locked   # refresh Cargo.lock, commit it

# 2. Prove the tag you are about to push is the tag this workspace needs.
scripts/verify-tag-version.sh v0.2.0

# 3. Run the release gate locally, on a real binary.
cargo build --release --locked -p forge-cli
scripts/release-gate.sh target/release/forge
#    -> release-gate-out/gate-summary.json, fsck-full.json, env-line.txt, ...

# 4. Merge to main, then tag and push. The tag is what triggers the release.
git tag -a v0.2.0 -m 'ForgeFS v0.2.0'
git push origin v0.2.0
```

To rehearse without publishing anything, run the workflow manually
(**Actions -> release -> Run workflow**) and leave `dry_run` ticked. Every gate
runs; `provenance` skips minting an attestation and `publish` is skipped
entirely. `dry_run` defaults to **true**, so a manual run can never publish by
accident.

## What the pipeline does, and in what order

```
verify-tag ................ tag == workspace.package.version, no toolchain, ~1 min
  |
  +-- gate (ubuntu-latest) ....... fmt / check / clippy -D warnings / test, 1.97.0
  +-- gate (macos-15) ............ the same, on the F_FULLFSYNC durability path
  +-- msrv (1.89.0) .............. check + test
  +-- supply-chain ............... cargo audit + cargo deny + Cargo.lock is clean
  +-- build (4 target triples) ... tarball + per-artifact sha256 + BUILD-INFO
        |
        +-- e2e-release-gate (3 triples) .. ForgeFS seals and verifies ITSELF
        +-- readonly-verify ............... NON-BLOCKING, red until #234 lands
  |
  +-- provenance ............ SLSA attestation over SHA256SUMS (publish only)
        |
        +-- publish ......... gh release create (skipped on dry runs)
```

`verify-tag` has no dependencies and needs no Rust toolchain, so the single most
common release footgun - a tag that does not match `workspace.package.version` -
costs about a minute to catch instead of forty.

`e2e-release-gate` depends on `build`, not on its own compile. Gating a rebuild
proves something about a rebuild. Gating the downloaded artifact proves
something about the bytes users will run: the job verifies the recorded SHA-256,
unpacks the tarball, asserts `forge --version` equals the released version, and
only then runs the gate against it.

`provenance` and `publish` depend on every blocking gate, so no attestation is
ever minted over bytes that failed one.

## What `scripts/release-gate.sh` asserts

One argument, a forge binary. Runs anywhere, not only in Actions.

| Phase | Assertion | Contract |
|---|---|---|
| `gate/init` | fresh forge initialises; `.forge/VERSION` is exactly `1` | FORMAT.md, I17 |
| `gate/grant` | three attenuated agent caps mint from the root cap | I13, I14 |
| `gate/two-agent-path` | 2 sessions, disjoint writes, 2 checkins, 2 merges; `main` advances; both contributions read back | README 60-second path |
| `gate/same-path-overlap` | overlap merge exits **4**, produces a **Conflict object** with distinct `ours`/`theirs` and the conflicting path, published under a typed `conflicts/main/*` ref; `main` does not move | I11, I7, W4 |
| `gate/stale-observation` | a disjoint checkin under a stale observation exits **4**; the destination ref does not advance; `main` does not advance; the overlay does not leak into `main`; a control agent's ref *does* advance | I9, W3 |
| `gate/seal-verify` | `seal main --tag <version> --attest` then `verify <version>`; the verified OID equals the sealed snapshot OID; `tags/<version>` is protected **and** sealed | I5, I7, I15 |
| `gate/fsck` | `fsck --full --json` parsed as **JSON**: `ok == true`, `full == true`, `findings == []`, positive counts | README, I15 |
| `gate/cli-abi` | every blocking row of the `CLI_ABI.md` exit-code table | CLI_ABI.md, #237 |
| `gate/environment-line` | the full `docs/BENCH.md` environment line is recorded | #24 |

The control assertion in `gate/stale-observation` matters: "the destination ref
did not advance" is only evidence if the probe that reads it can *see* an
advance. So a third agent checks in immediately afterwards and its ref must
move. A negative assertion with no positive control is a test that passes when
the harness is broken.

The gate writes its artifacts, including on failure, and the workflow uploads
them with `if: always()`. A red release is diagnosable from the artifacts alone.

## The CLI ABI table and `known_failing`

`scripts/cli-abi-conformance.sh` is the conformance test #237 says is missing.
Each row encodes the **contract** in `CLI_ABI.md`, never today's behaviour.

Ten rows are contract-correct but currently violated, so they sit in a
`known_failing` set that is reported and non-blocking. Every one was reproduced
against a real `forge` binary before being written down:

| Row | Contract | Observed | Why |
|---|---|---|---|
| `abi/1-duplicate-branch-name` | 1 | 5 | an existing ref name hits the `refs` PRIMARY KEY and surfaces as `Error::Sqlite` |
| `abi/2-duplicate-seal-tag` | 2 | 5 | re-sealing a frozen tag hits the `seals` PRIMARY KEY and surfaces as `Error::Sqlite` |
| `abi/1-non-utf8-cap-file` | 1 | 5 | `load_cap` uses `read_to_string`, so non-UTF-8 `--cap` bytes surface as `Error::Io` |
| `abi/1-write-file-missing` | 1 | 5 | a `--file` path the caller got wrong surfaces as `Error::Io` |
| `abi/1-import-not-a-directory` | 1 | 5 | importing a plain file where a directory is required surfaces as `Error::Io` |
| `abi/1-unknown-subcommand` | 1 | **2** | clap's usage error bypasses `error_exit_code()` |
| `abi/1-unknown-flag` | 1 | **2** | same |
| `abi/1-log-unknown-ref` | 1 | **0** | `log` of a ref that does not exist exits 0 and prints nothing |
| `abi/1-landmark-absent-oid` | 1 | **0** | exits 0, prints a success line, and persists a `landmarks` row for an object that does not exist |
| `abi/1-mount-unknown-ref` | 1 | **0** | `mount` accepts a ref that does not exist |

Two of these deserve to be read as more than exit-code pedantry.

**clap usage errors exit 2, which `CLI_ABI.md` defines as "corruption or
sealed-state violation."** An agent that mistypes a subcommand is therefore
indistinguishable, to automation keyed on exit codes exactly as `CLI_ABI.md`
instructs, from a repository whose durable bytes are corrupt. The fix is a clap
error mapping in `main()`, so usage errors land in class 1.

**`mount` of a non-existent ref exits 0 and then poisons `fsck`.** After that
mount, `fsck --full` reports `[MOUNT_REF]` and exits 2 on a repository whose
bytes are entirely intact. Any holder of read+branch authority can therefore
make a release gate keyed on `fsck`/`verify` fail with no corrupt byte anywhere.
Fixing either side resolves the pair: `mount` rejects the missing ref (exit 1),
or `fsck` stops classing a dangling mount as corruption. This pipeline is not
exposed to it - `release-gate.sh` builds its own forge and never mounts - but a
gate that ran `fsck` over an operator-supplied repository would be.

The marker is self-cleaning. A `known_failing` row that starts *matching* its
contract is reported as a hard error - "stale known_failing row" - which fails
the gate until someone deletes the marker. Fixing #237 therefore promotes these
rows to blocking automatically; nobody has to remember.

One row, `abi/3-busy` (exit 3, transient contention), is declared
`unexercised` rather than faked: producing it deterministically needs a second
process holding a SQLite write transaction past `busy_timeout`, which is the
same evidence class as #147. The table records the gap instead of pretending
coverage.

## Build and cross-compilation choices

| Triple | Runner | How |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | native |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | **native** arm64 runner |
| `x86_64-apple-darwin` | `macos-15` | cross, same universal macOS SDK |
| `aarch64-apple-darwin` | `macos-15` | native |

`rusqlite` is used with `bundled`, so SQLite is compiled from C source by the
`cc` crate on every build. That is why `aarch64-unknown-linux-gnu` builds on a
native arm64 runner rather than through a glibc cross toolchain: a cross setup
would put a second, unvalidated C toolchain on the durability-critical path.
`x86_64-apple-darwin` is a genuine cross build, but Apple's SDK is universal and
clang accepts `-target x86_64-apple-darwin` with no extra toolchain, so the same
SDK compiles bundled SQLite for both macOS arches.

### Why the Linux artifacts are glibc-dynamic

A statically linked musl artifact would be more portable, and it is *one matrix
row away* - but it is not a free swap. Building for `*-linux-musl` replaces the
C compiler that builds bundled SQLite (`musl-gcc` instead of the system GCC),
which changes the durability-critical code being shipped. This pipeline exists
to guarantee that published bytes verified themselves, so a musl artifact needs
its own `e2e-release-gate` row before it can be published at all. Shipping an
ungated musl binary alongside gated glibc binaries would quietly break the one
property the whole pipeline provides.

Until then, each artifact ships a `BUILD-INFO.txt` recording `ldd --version`
(the glibc floor), the runner image, the exact build command, the toolchain, and
the binary's dynamic dependencies - so a consumer can tell whether it will run
before downloading 4 MB to find out.

Binaries are **not** stripped. Symbols are worth their size when triaging a
crash in a storage engine.

## Supply chain (`deny.toml`, #45)

```bash
cargo audit --deny warnings
cargo deny --locked check advisories bans licenses sources
```

- `licenses.allow` lists **exactly** the five license classes the current graph
  needs: `Apache-2.0`, `MIT`, `BSD-3-Clause` (the ed25519 signing path),
  `Unicode-3.0` (AND-ed by `unicode-ident`, not OR-ed) and `Zlib` (`foldhash`).
  A new class failing this gate is the gate working. Decide about it in its own
  PR, per SECURITY.md's dependency-upgrade rules; do not widen the list to
  unblock a release.
- `advisories.yanked = "deny"`, `advisories.ignore = []`.
- `sources` allows crates.io only. A git dependency or private registry on the
  trusted path is a release-blocking event.
- `bans.multiple-versions = "warn"`: duplicate transitive versions are hygiene,
  not a security boundary, and denying them lets an unrelated upstream bump veto
  a release.
- `bans.wildcards = "warn"`, and this one is a **real finding worth acting on**.
  cargo-deny's `allow-wildcard-paths` escape hatch only applies to crates marked
  `publish = false`. Every `forge-*` member is publishable, so its own
  intra-workspace `{ path = ... }` dependencies are all reported as wildcards.
  crates.io would reject those manifests for the same reason. Giving each
  `[workspace.dependencies]` `forge-*` entry a `version` alongside its `path`
  fixes both problems at once and lets `wildcards` become `deny`.

The `supply-chain` job additionally asserts that **the build did not rewrite
`Cargo.lock`**: every `cargo` invocation in the workflow passes `--locked`, and a
dedicated step fails if `git` reports the lockfile dirty afterwards.

## Read-only media (#234)

`readonly-verify` builds a 256 MiB ext4 loopback filesystem, initialises and
seals a real repository on it, remounts it read-only, and then runs
`fsck --full`, `fsck --full --json` and `verify`. All three are read-only paths
by contract - README says `fsck` is read-only, and I15 says verify/fsck reread
durable bytes - so all three must exit 0.

Today they do not, which is #234. The job is therefore `continue-on-error: true`
and **nothing depends on it**, so a knowingly-red check never sits on a blocking
path. When #234 lands, the job goes green on its own; at that point delete the
`continue-on-error:` line and add `readonly-verify` to `provenance.needs`. It
becomes a real gate for the cost of deleting one line.

The likely shape of the fix, for whoever picks up #234: the cell lock, the
SQLite WAL side files (`-wal`/`-shm`) and the object-directory re-proof in
`Store::open` all currently want write access. A read-only open needs an
explicit read-only mode that takes no cell lock, opens SQLite with
`mode=ro`/`immutable=1`, and skips durability re-proof because nothing can be
published.

## Actions are pinned by commit SHA

Every action is pinned to a commit with its tag in a trailing comment:

```yaml
uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
```

A release workflow is exactly where a movable tag is a supply-chain hole (#45),
and `trusted-pr-integration.yml` already pins `actions/checkout` to this same
commit. Note that `ci.yml`, `fuzz.yml` and `security.yml` still float on tags;
pinning them is a separate, mechanical PR (`pinact`, or `gh api
repos/OWNER/REPO/commits/TAG --jq .sha`) and should be done for the whole
repository at once. A release workflow pinned while everything around it floats
is a partial mitigation, not a solved problem.

## Injection hygiene

No `${{ github.event.* }}`, `${{ github.ref_name }}`, or any other
attacker-influenced expression is interpolated into a `run:` body anywhere in
`release.yml`. Every such value crosses into the shell through `env:` and is
referenced as a quoted shell variable. Git refnames legitimately accept `$`,
backtick, `;`, `&`, `|` and parentheses, so a tag is untrusted input.

`verify-tag` additionally constrains the tag to
`^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$` before any downstream
job uses it, which removes shell metacharacters from the value entirely. Build
metadata (`+...`) is rejected rather than half-handled.

Capability tokens are authority (I13/I14). `release-gate.sh` and
`cli-abi-conformance.sh` redact every `fmac1_*` token out of logs and artifacts,
including throwaway fixture tokens.

## Permissions

Top-level `permissions: contents: read`. Elevated only where needed:

| Job | Extra permission | Why |
|---|---|---|
| `provenance` | `id-token: write`, `attestations: write` | mint the SLSA attestation |
| `publish` | `contents: write` | `gh release create` |

Every other job is read-only, and every `actions/checkout` uses
`persist-credentials: false` because no job pushes with git.

## Timeouts

Every job sets `timeout-minutes`. The current values are conservative first-cut
budgets sized for a 2 vCPU hosted runner, not measurements:

| Job | Budget | Note |
|---|---|---|
| `verify-tag` | 10 | no toolchain; should finish in ~1 min |
| `gate` | 35 | `ci.yml` proves 20 (Linux) / 25 (macOS) |
| `msrv` | 30 | |
| `supply-chain` | 45 | `cargo install cargo-audit` **and** `cargo-deny` from source, cold `rust-cache` bin cache |
| `build` | 40 | includes compiling bundled SQLite per target |
| `e2e-release-gate` | 15 | the gate is ~1.4 s on a 2 vCPU Linux box and ~10 s on an M1 Pro (both measured); this job compiles nothing, so the budget is download + overhead |
| `readonly-verify` | 20 | |
| `provenance` | 20 | |
| `publish` | 20 | |

Tighten these once real step timings exist. Prefer a budget that catches a hang
over one that flakes on a slow runner.

## Files

| Path | Role |
|---|---|
| `.github/workflows/release.yml` | the pipeline |
| `scripts/release-gate.sh` | ForgeFS seals and verifies itself; the release's substance |
| `scripts/cli-abi-conformance.sh` | the `CLI_ABI.md` exit-code conformance table (#237) |
| `scripts/forge-env-line.sh` | the `docs/BENCH.md` environment line (#24), reusable by benchmark runs |
| `scripts/verify-tag-version.sh` | tag vs `workspace.package.version`, plus member inheritance |
| `deny.toml` | cargo-deny policy (#45) |

All four scripts must be committed with mode `100755`:

```bash
git update-index --chmod=+x scripts/release-gate.sh \
  scripts/cli-abi-conformance.sh scripts/forge-env-line.sh \
  scripts/verify-tag-version.sh
```

`verify-tag` asserts this in its first step, so a lost exec bit fails in about a
minute with the fix in the error message rather than as an opaque
"permission denied" thirty minutes later.
