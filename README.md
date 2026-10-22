# ForgeFS

Isolated, auditable workspaces for concurrent coding agents. Built in Rust.

Each agent works from a pinned snapshot. Publishing uses compare-and-swap: one agent advances
the ref; a losing agent keeps its completed work in an explicit fork. Recorded reads also let
ForgeFS refuse a checkin when a dependency the agent observed has changed. Sealed releases can
be verified from durable object bytes.

Use ForgeFS when several agents write to one repository and you need provenance, explicit
conflicts, and evidence of exactly what shipped. For one agent and one checkout, Git is simpler.

[Install](#install) · [Try it](#try-it) · [Evidence and cost](#evidence-and-cost) ·
[Limits](#limits) · [Full reference](docs/REFERENCE.md)

## Install

The current release is **v0.4.0**, with binaries for Linux and macOS on x86-64 and ARM64.
There is no Windows build, and the source requires Unix APIs.

Choose a target: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, or `aarch64-apple-darwin`. On Linux, with `curl`, `tar`, and `sha256sum`:

```bash
V=0.4.0; T=x86_64-unknown-linux-gnu
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/forge-$V-$T.tar.gz
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf forge-$V-$T.tar.gz
sudo install forge-$V-$T/forge /usr/local/bin/
forge --version
```

On macOS, set `T` to the matching Apple target and use `shasum -a 256 --ignore-missing -c SHA256SUMS`
in place of `sha256sum`. A bare Debian image first needs `apt-get install curl ca-certificates`.
The final `install` needs root. For provenance verification, install and authenticate the GitHub CLI,
then run `gh attestation verify forge-$V-$T.tar.gz -R zozo123/forgefs`.

Read [upgrade and migration notes](docs/REFERENCE.md#which-build-this-page-documents) before
opening an existing repository. The v0.2.1 catalog migration is one-way; v0.4.0 also renames
session fork refs. [Release evidence and supply-chain details](docs/SUPPLY-CHAIN.md).

## Try it

In a fresh directory, import a file, edit it in an isolated session, publish, and verify the store:

```bash
mkdir -p project
printf 'hello\n' > project/hello.txt
forge init ./demo
export FORGE_DIR="$PWD/demo"
ROOT="$FORGE_DIR/.forge/keys/root.cap"
forge --cap "$ROOT" import --ref work ./project

SESSION=$(forge --cap "$ROOT" session open --from=work)
forge --cap "$ROOT" mount --ns "$SESSION" / ref:work --rw
forge --cap "$ROOT" read --ns "$SESSION" /hello.txt
forge --cap "$ROOT" write --ns "$SESSION" /hello.txt --text 'hello from an agent'
forge --cap "$ROOT" checkin --ns "$SESSION" -m 'update greeting'
forge --cap "$ROOT" fsck --full
```

This first run uses the root capability. For agent-scoped permissions and the interesting cases,
follow the [two-agent walkthrough](docs/REFERENCE.md#a-worked-example): a publication race,
a preserved fork, a stale-read refusal, and a verified seal. Both winning and forked checkins
exit 0; inspect the result to distinguish them. Stale observations and merge conflicts exit 4.
[CLI output and exit-code contract](CLI_ABI.md).

## Evidence and cost

[INVARIANTS.md](INVARIANTS.md) maps 24 correctness rules to executable evidence.
The [full reference](docs/REFERENCE.md#why-you-should-believe-any-of-this) includes gate commands
and recorded output; [release artifacts](https://github.com/zozo123/forgefs/releases) publish
verification evidence beside the binaries.

ForgeFS pays for isolation, recorded reads, and durable publication. In the recorded same-filesystem
comparison, the CLI delivered **131.4 ops/s**, Git worktrees **344.9 ops/s**, and Git with
`core.fsync=all` **254.7 ops/s**. These are medians from nine fresh repositories per configuration
on one shared Linux VM, with 32 agents and four workers. The durability barriers differ, so these
are protocol-specific throughput observations, not equivalent-durability speed ratios.
See the [environment and results](docs/REFERENCE.md#what-it-costs) and
[benchmark protocol](docs/BENCH.md) before quoting them.

## Limits

- **Single node, Unix only.** No replication, remote transport, or multi-host consensus.
- **Not a POSIX filesystem.** Symlinks are refused by default; following them copies their targets.
  Only the executable bit survives import/export. Permissions, ownership, and timestamps are lost
  or widened. See [POSIX boundaries](docs/POSIX.md).
- **Whole blobs must fit in memory.** A measured 256 MiB read peaked at about 3× payload RSS.
  There is no chunked-file representation; see [chunking](docs/CHUNKING.md).
- **Graph walks are bounded.** The default ceiling is one million objects; larger reachable sets
  need an explicit limit increase and matching RAM. Object files also consume filesystem inodes.
- **Crash evidence is process-level.** Fault injection and real SIGKILL tests do not establish
  power-loss behavior; see the [recovery contract](docs/RECOVERY.md).

The [complete limits](docs/REFERENCE.md#limits) also document staged-directory listing gaps,
checkin/abandon inconsistencies, daemon coverage, and upgrade edge cases.

## Code and documentation

| Area | Crates |
|---|---|
| Immutable objects and storage | `forge-types`, `forge-core`, `forge-store` |
| Authority and isolated sessions | `forge-cap`, `forge-ns` |
| Integration and process boundaries | `forge-merge`, `forge-protocol`, `forge-api`, `forge-cli` |

[Full crate map](docs/REFERENCE.md#layout) · [Object format](FORMAT.md) ·
[Recovery](docs/RECOVERY.md) · [Conflicts](docs/CONFLICTS.md) · [Garbage collection](docs/GC.md) ·
[Security](SECURITY.md) · [Contributor rules](AGENTS.md)

Apache-2.0. See [LICENSE](LICENSE).
