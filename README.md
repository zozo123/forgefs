# ForgeFS

Isolated, auditable workspaces for concurrent coding agents. Built in Rust.

For teams running several coding agents on one repository. Each agent works in a session pinned to
one snapshot. Publishing is compare-and-swap on a named ref: one agent advances it, and a loser's
completed work is kept in an explicit fork, not overwritten. Reads are recorded, so a checkin is
refused when a dependency the agent read has since changed, even if its own writes touch nothing in
common. Releases are ed25519-sealed snapshots that `forge verify` re-hashes from disk. For one agent
and one checkout, Git is simpler.

ForgeFS is a content-addressed (BLAKE3) store driven by the `forge` CLI or a local socket daemon
(`forge serve`). It is not a kernel/FUSE mount and not a Git backend: `forge import` brings a tree
in, `forge export` writes a tar out.

**Status:** Pre-1.0. Security fixes land on `main` only. Docs on `main` may describe unreleased
verbs (e.g. `forge receipt`); the [v0.4.0 tag](https://github.com/zozo123/forgefs/tree/v0.4.0) has
the released docs. The [full reference][ref] has the detail behind this page.

## Install

The current release is **v0.4.0**, for Linux and macOS on x86-64 and ARM64 (`x86_64-` or `aarch64-`
followed by `unknown-linux-gnu` or `apple-darwin`). There is no Windows build. On Linux:

```bash
V=0.4.0; T=x86_64-unknown-linux-gnu
curl -fsSLO https://github.com/zozo123/forgefs/releases/download/v$V/forge-$V-$T.tar.gz
curl -fsSLO https://github.com/zozo123/forgefs/releases/download/v$V/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf forge-$V-$T.tar.gz
sudo install forge-$V-$T/forge /usr/local/bin/
forge --version
```

On macOS, set `T` to the Apple target and use `shasum -a 256 --ignore-missing -c SHA256SUMS`.
A bare Debian image first needs `apt-get install curl ca-certificates`. For provenance, run
`gh attestation verify forge-$V-$T.tar.gz -R zozo123/forgefs` with an authenticated GitHub CLI
([release evidence and supply chain](docs/SUPPLY-CHAIN.md)). To build `main` from source instead
(Rust 1.89 or newer, `git`, and a C compiler), run
`cargo install --locked --git https://github.com/zozo123/forgefs forge-cli`.

## Try it

Two agents check in to `work` from the same snapshot; then one reads a dependency that moves before
its next checkin. In an empty directory, in bash or zsh:

```bash
mkdir -p app dep && echo v1 > app/main.txt && echo 'API = 1' > dep/api.txt
forge init ./demo
export FORGE_DIR="$PWD/demo" FORGE_CAP="$PWD/demo/.forge/keys/root.cap"
forge import --ref work ./app && forge import --ref deps ./dep
A=$(forge session open --from=work); forge mount --ns "$A" / ref:work --rw
B=$(forge session open --from=work); forge mount --ns "$B" / ref:work --rw
forge write --ns "$A" /a.txt --text alice; forge write --ns "$B" /b.txt --text bob
forge checkin --ns "$A" -m alice
forge checkin --ns "$B" -m bob; echo "exit $?"
forge mount --ns "$A" /vendor ref:deps; forge read --ns "$A" /vendor/api.txt
echo 'API = 2' > dep/api.txt && forge import --ref deps ./dep
forge write --ns "$A" /a.txt --text 'alice v2'
forge checkin --ns "$A" -m 'alice v2'; echo "exit $?"
forge fsck --full
```

What just happened: alice published (`updated work …`); bob lost the compare-and-swap and got
`forked work -> heads/agents/anon/forks/work/…` with `exit 0`, his work kept and hers intact (check
that line, not the status); alice's next checkin got `stale observation of /vendor:/api.txt` and
`exit 4`, because `deps` moved after she read it, though she wrote only `/a.txt`. This uses the root
capability. Agent-scoped capabilities, merging the fork, and sealing a release
are in the [two-agent walkthrough][walk]. Output and exit codes: [CLI_ABI.md](CLI_ABI.md).

**Upgrading?** The [v0.2.1 catalog migration][upgrade021] is one-way, and v0.4.0
[renames session fork refs][forkref] from `forks/<ref>/<agent>/<ulid>` to
`heads/agents/<agent>/forks/<ref>/<ulid>`. Read both before opening an older repository.

## Evidence and cost

[INVARIANTS.md](INVARIANTS.md) maps every numbered rule (I1–I25 on main; v0.4.0 shipped I1–I24)
to code and tests. The [full reference][believe] includes gate commands and recorded output;
[release artifacts](https://github.com/zozo123/forgefs/releases) publish verification evidence.

ForgeFS pays for isolation, recorded reads, and durable publication; on the measured box it is
slower than Git. In the recorded same-filesystem comparison (32 agents, four workers, medians of
nine fresh repositories per configuration, one shared Linux VM), the `forge` CLI delivered
**131.4 ops/s**, Git worktrees **344.9 ops/s**, and Git with `core.fsync=all` **254.7 ops/s**.
Before quoting them:

- Even in-process ForgeFS (`forge bench`, no execs) reached only **245.8 ops/s**, below stock Git.
- The CLI row costs 3 execs per agent operation; Git's rows cost 2.
- Repetitions within one configuration varied by up to 1.8× ([raw runs](docs/bench-results/w7-5a7017a/)).
- The durability barriers differ, so these are protocol-specific throughput observations, not
  equivalent-durability speed ratios. See the [environment and results][cost] and
  [benchmark protocol](docs/BENCH.md).

## Limits

- **Single node, Unix only.** No replication, remote transport, or multi-host consensus.
- **Not a POSIX filesystem.** Symlinks are refused by default; following them copies their targets.
  Only the executable bit survives import/export. Permissions, ownership, and timestamps are
  dropped or widened silently. See [POSIX boundaries](docs/POSIX.md).
- **Whole blobs must fit in memory.** A measured 256 MiB read peaked at about 3× payload RSS.
  There is no chunked-file representation; see [chunking](docs/CHUNKING.md).
- **One object is one file.** On a 30 GB ext4 volume, importing a 1M-file tree ran out of inodes at
  685,044 objects with 45% of bytes free (exit 5; repository intact). Size by inodes. fsck, gc and
  seal verification hold the whole reachable graph in memory, capped at 1,000,000 objects by default
  (exit 1; raise with `FORGEFS_MAX_GRAPH_OBJECTS`, about 100 B RAM per object). The walk is not
  resumable.
- **Power loss is modelled, not proven.** A Linux-only libc write/fsync replay, added on `main` after
  v0.4.0 (CI `power-loss` job, [docs/DURABILITY.md](docs/DURABILITY.md)), drops un-fsynced bytes at
  sampled cut points. It does not model drive caches, sector tearing or filesystem bugs; see the
  [recovery contract](docs/RECOVERY.md).

The [complete limits][limits] also document: no W6 results (do not infer tree scaling from the
32-agent numbers); death by signal (SIGBUS on a full disk) carries no exit code; the gate scripts'
undeclared-command backstop needs bash ≥ 4; staged-directory listing gaps; checkin/abandon
inconsistencies; daemon coverage; and upgrade edge cases.

## Code and documentation

Objects and storage: `forge-types`, `forge-core`, `forge-store`. Authority and sessions: `forge-cap`,
`forge-ns`. Integration and boundaries: `forge-merge`, `forge-protocol`, `forge-api`, `forge-cli`.

[Full crate map][layout] · [Object format](FORMAT.md) · [Conflicts](docs/CONFLICTS.md) ·
[Garbage collection](docs/GC.md) · [Security](SECURITY.md) · [Contributor rules](AGENTS.md) ·
Apache-2.0, see [LICENSE](LICENSE).

[ref]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md
[walk]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#a-worked-example
[upgrade021]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#upgrading-a-v021-repository
[forkref]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#the-fork-ref-rename--the-one-breaking-change
[believe]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#why-you-should-believe-any-of-this
[cost]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#what-it-costs
[limits]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#limits
[layout]: https://github.com/zozo123/forgefs/blob/main/docs/REFERENCE.md#layout
