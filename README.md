# ForgeFS

Many autonomous agents, one repository, no shared mutable checkout.

ForgeFS is a content-addressed filesystem. Bytes are immutable and named by their BLAKE3 hash. Each
agent works in a session pinned to one snapshot, so nothing it reads can move under it. Every read
is recorded as a fact. Publication is compare-and-swap on a named ref, so two agents cannot silently
overwrite one another: one wins, and the loser gets an explicit fork that still holds its work. An
agent that reasoned about a file someone else has since changed is refused, even when its own writes
touch nothing in common. Releases are ed25519-sealed snapshots that re-verify from durable bytes.

## Is this your problem?

Two or more of these should be true before ForgeFS is worth its cost:

- More than one agent writes to one repository at the same time.
- You need to know, from the store itself and after the fact, which agent produced which bytes.
- A silent overwrite is worse for you than a loud failure.
- You must be able to prove later exactly what shipped.

If you have one agent and one checkout, use Git. It is smaller, you already know it, and on the box
measured below a `forge` CLI checkin has lower raw throughput than a `git commit` in a worktree (see
[What it costs](#what-it-costs) — the comparison is not durability-equivalent in Git's favour).
ForgeFS buys isolation, recorded reasoning, and provenance. It costs durability barriers per
publication, and it is not a POSIX filesystem.

## Install

Released binaries exist for four targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. **There is no Windows build and the source does not
compile on Windows** — production code uses `std::os::unix` unconditionally.

The install below needs `curl`, `tar`, `sha256sum` (`shasum` on macOS) and root for the final
`install`. A bare `debian:bookworm` has `tar` and `sha256sum` but **not** `curl`; `apt-get install
curl ca-certificates` first, or the first line fails with `curl: command not found`.

```bash
V=0.4.0; T=x86_64-unknown-linux-gnu
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/forge-$V-$T.tar.gz
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf forge-$V-$T.tar.gz
sudo install forge-$V-$T/forge /usr/local/bin/
forge --version
```

Only `$V` moves — and the transcripts below are `V=0.3.0` runs on Debian 12, deliberately. **This
file ships inside the v0.4.0 tarball, so it cannot quote v0.4.0's own checksums or attestation: they
do not exist until after the artifact containing this sentence has been built.** What is shown is
therefore the previous release, executed rather than remembered, with only `$V` differing from what
you will run. Those exact commands printed:

```text
forge-0.3.0-x86_64-unknown-linux-gnu.tar.gz: OK
forge 0.3.0
```

On macOS the same commands work with `shasum -a 256 --ignore-missing -c SHA256SUMS`. The tarball
holds the binary and four documents, and nothing else:

```text
forge-0.3.0-x86_64-unknown-linux-gnu/
forge-0.3.0-x86_64-unknown-linux-gnu/README.md
forge-0.3.0-x86_64-unknown-linux-gnu/LICENSE
forge-0.3.0-x86_64-unknown-linux-gnu/INVARIANTS.md
forge-0.3.0-x86_64-unknown-linux-gnu/CLI_ABI.md
forge-0.3.0-x86_64-unknown-linux-gnu/forge
```

`SHA256SUMS` for v0.4.0 has 41 entries — the v0.3.0 file checked above has 36, because v0.4.0's
payload catalog ([`.github/scripts/release-assets.sh`](.github/scripts/release-assets.sh)) adds a
CycloneDX SBOM per target and a reproducibility report. It covers the four binaries *and* the release-gate evidence published
beside them — per target: a build-info file, a CycloneDX SBOM, the ABI conformance table, the
Conflict-object record, the environment line (as both `.txt` and `.json`), the `fsck --full`
report, the gate summary and the seal attestation, plus the double-build reproducibility evidence
for `x86_64-unknown-linux-gnu` (see [`docs/SUPPLY-CHAIN.md`](docs/SUPPLY-CHAIN.md)). Each release
is also covered by a SLSA provenance attestation you can check without trusting this page:

```bash
gh attestation verify forge-$V-$T.tar.gz -R zozo123/forgefs
```

```text
Loaded digest sha256:87a743f0402c0d0fa367e1a6c265c6127be6d72f66f6c46a2229efcb28dabdff for file://forge-0.3.0-x86_64-unknown-linux-gnu.tar.gz
Loaded 1 attestation from GitHub API

The following policy criteria will be enforced:
- Predicate type must match:................ https://slsa.dev/provenance/v1
- Source Repository Owner URI must match:... https://github.com/zozo123
- Source Repository URI must match:......... https://github.com/zozo123/forgefs
- Subject Alternative Name must match regex: (?i)^https://github.com/zozo123/forgefs/
- OIDC Issuer must match:................... https://token.actions.githubusercontent.com

✓ Verification succeeded!

The following 1 attestation matched the policy criteria

- Attestation #1
  - Build repo:..... zozo123/forgefs
  - Build workflow:. .github/workflows/release.yml@refs/tags/v0.3.0
  - Signer repo:.... zozo123/forgefs
  - Signer workflow: .github/workflows/release.yml@refs/tags/v0.3.0
```

That is a real run against `forge-0.3.0-x86_64-unknown-linux-gnu.tar.gz` on gh 2.83.1. Two things
about it, both measured rather than assumed:

- **It needs a GitHub token.** An earlier version of this page said it did not, because attestations
  are public. That is wrong about the command: run with no credentials on gh 2.63.2 *and* on gh
  2.98.0, `gh attestation verify` exits **4** with `To get started with GitHub CLI, please run: gh
  auth login`. `gh auth login` or `GH_TOKEN` first; any token works, it need not have rights to this
  repository.
- **It prints that only to a terminal.** Redirected to a pipe or a file it writes nothing and
  communicates through its exit status alone — the transcript above was captured through a pty.
  Check `$?`, not the output. The exact layout is also gh-version dependent; what does not vary is
  the exit status.

### Which build this page documents

`v0.4.0` is the current release and everything on this page is in it. Several sections document
behaviour that **postdates v0.3.0**, so on that older release they do not work. Checked against both
binaries — the released `forge 0.3.0` and the v0.4.0 build:

| Documented here | In v0.3.0? | In v0.4.0? |
|---|---|---|
| session forks at `heads/agents/<agent>/forks/<ref>/<ulid>` | no — `forks/<ref>/<agent>/<ulid>` | **yes, this is a rename** |
| `mount --rw` refused at mount time for a protected ref, an `oid:` spec, or a ref not holding a commit | partly — only the `oid:` and non-commit cases | yes, protected refs too |
| `checkin --mount <name>` refuses a name this session has no mount for | no — it published the default `/` mount and said `updated` | yes, exit 1 |
| `forge mv` — an atomic move inside one mount (I24) | no — the verb does not exist | yes |
| a read taken through one mount still constrains the session after another mount is checked in | no — the first checkin of any mount forgot it (#329) | yes |
| a repository too large for the object-graph walk is `invalid`, not `corrupt` | no — exit 2, `object graph exceeded 1000000 objects` | yes, exit 1 |
| `seal` CASes the ref it names and refuses a moved head | no — it silently sealed the pre-race commit | yes, exit 4 |
| `serve` refuses unknown ops and fields, and answers structured JSON | no — unknown fields were silent defaults, `ns.checkin` returned a Rust `Debug` string | yes |
| `fsck` on an un-migrated catalog refuses at exit 1 instead of reporting corruption at exit 2 | no — `FAILED (full)`, exit 2, `CATALOG_SCHEMA` + `SCHEMA_LEDGER` findings | yes |
| `fsck --full --json` emits a refusal *document* when it refuses | no — it emitted a full audit report with `"ok": false` and two findings against an intact repository | yes |
| oversized caller input is exit 1 `invalid`, not exit 2 `corrupt` | no | yes |
| `gc` names both modes, and `--dry-run`/`--collect` agree on reachability | no | yes |
| the gate scripts need no `python3` and no `grep`, and exit 3 for a missing prerequisite | no — the v0.3.0 tree's gates required `python3` (exit 2 without it) and `grep` | yes |
| everything else on this page | yes | yes |

**If you have tooling that matches fork paths, read the next section before upgrading.**

### The fork ref rename — the one breaking change

A session that loses a publication CAS still keeps its work, in a fork ref. In v0.3.0 that ref was:

```text
forks/<ref>/<agent>/<ulid>
```

In v0.4.0 it is:

```text
heads/agents/<agent>/forks/<ref>/<ulid>
```

Anything that globbed `forks/*` to find an agent's parked work sees nothing now. `refs` output, the
`forked …` line a losing checkin prints, and `abandon fork` all use the new form. Both namespaces
still exist — merge and import forks stay under `forks/`, session forks moved — and `abandon fork`
names both when you hand it something that is neither:

```text
forge: invalid: only fork refs may be abandoned (forks/* or heads/agents/<agent>/forks/*), not main
```

The reason for the move is [I13](INVARIANTS.md), not tidiness. A capability may only ever be
attenuated, so a losing agent's token cannot be widened to cover a ref outside its subtree; parking
the work under `heads/agents/<agent>/` puts it inside the namespace that agent's token already
covers. Minting a token that covered `forks/**` at session-open time would have made `session open`
an authority-amplification primitive, which is exactly what I14 forbids. Issue #343 has the argument.

Both binaries, same race, same repository shape. v0.3.0:

```text
forked work -> forks/work/bob/01M0YRYAW1P3NEFWZA38DSQ5J5 ours=a06242c72eb2925e17e21091a0b262884c688f5a2dc2ed90cd2dc46003522fca theirs=ed3b70555c350af0a1811635a13fab8bfcaf5304238161b7b567cde105f4bbf8
-- commit                           forks/work/bob/01M0YRYAW1P3NEFWZA38DSQ5J5 a06242c72eb2925e17e21091a0b262884c688f5a2dc2ed90cd2dc46003522fca
```

v0.4.0:

```text
forked work -> heads/agents/bob/forks/work/01M0YQ0TCK7ADGPC5XY5E6V9JS ours=4604cad685a967ca767501dc49b4e04aa80b627477db2113c587efba774c9b58 theirs=01a7342e30a8b944ed0cc50ec7fd7b22c442f94883e897042ad6597cc98f0de0
-- commit                           heads/agents/bob/forks/work/01M0YQ0TCK7ADGPC5XY5E6V9JS 4604cad685a967ca767501dc49b4e04aa80b627477db2113c587efba774c9b58
```

Migration is a glob change; nothing in the store needs converting, because a fork ref created by
v0.3.0 keeps the name it was created with and `abandon fork` still accepts it.

### Upgrading a v0.2.1 repository

The first **read-write** open of a v0.2.1 repository by a v0.3.0-or-later binary migrates its
metadata catalog from schema v2 to v3 in place. Any ordinary command does it; `forge refs` is
enough. The schema is still v3 in v0.4.0, so a repository already used by v0.3.0 needs nothing.

The migration never rewrites an object file or moves an ObjectId (I17). Re-verified for this
release: a repository written by the released v0.2.1 binary, sha256 over every object file before
and after, byte-identical, `.forge/VERSION` still `1`, `fsck --full` clean afterwards.

Two things to know before you run anything:

- **Run a read-write command first.** `forge fsck --full` on a repository that has *not* been
  migrated refuses, exit **1**, naming the version it found, the version it needs, and the remedy:

  ```text
  forge: invalid: metadata schema version 2 needs migration to 3, which a read-only check cannot perform; fsck will not migrate a repository it was asked to diagnose. Open the repository once for writing to migrate it (for example `forge --dir <repo> --cap <cap> refs`), then re-run `forge fsck --full`
  ```

  It will not migrate behind your back, and it will not call an intact old repository corrupt —
  exit 2 is reserved for corruption ([`CLI_ABI.md`](CLI_ABI.md)), and a healthy v0.2.1 repository is
  not corrupt (issue #348). `forge verify` and `forge fsck` without `--full` refuse the same way.
  One read-write open and `fsck --full` returns `ok (full): 2 refs, 5 objects, 0 namespaces`.

  **This refusal is new in v0.4.0.** The released v0.3.0 binary, on the same un-migrated repository,
  reports corruption instead — `forge: corrupt: fsck found 2 problem(s)`, exit **2**, with
  `CATALOG_SCHEMA` and `SCHEMA_LEDGER` findings against a repository that is entirely intact. If you
  are on v0.3.0 and `fsck` calls your v0.2.1 repository corrupt, that is issue #348 and not your
  data.

  With `--json` the v0.4.0 refusal is a *document*. It carries no `ok` field and no counters,
  deliberately, so nothing can read it as an audit that found nothing (issue #356):

  ```json
  {
    "schema": "forgefs.fsck-refusal/1",
    "audited": false,
    "reason": "schema_needs_migration",
    "schema_version": 2,
    "supported_schema_version": 3,
    "detail": "metadata schema version 2 needs migration to 3, which a read-only check cannot perform; fsck will not migrate a repository it was asked to diagnose. Open the repository once for writing to migrate it (for example `forge --dir <repo> --cap <cap> refs`), then re-run `forge fsck --full`"
  }
  ```

  The prose still goes to stderr and the exit code is still 1, so no consumer's branch on status
  changes.
- **The migration is one-way.** A v0.2.1 binary pointed at a migrated repository fails closed with
  `forge: invalid: metadata schema version 3 is newer than supported 2`, exit 1. Objects stay
  readable by any VERSION 1 reader, but the catalog does not go back. Copy the repository first if
  you need to roll back.

If you want to track `main` rather than a release:

```bash
cargo install --locked --git https://github.com/zozo123/forgefs forge-cli
```

That needs a Rust toolchain, `git`, and a C toolchain for the linker — on a bare Debian,
`apt-get install git build-essential` and then rustup. Any stable `rustc` at or above the 1.89 MSRV
works; `cargo install --git` selects your default toolchain and does not honour the repository's
`rust-toolchain.toml`, and it tracks whatever `main` is when you run it.

Or clone and build, which is what the gate commands further down assume:

```bash
git clone https://github.com/zozo123/forgefs && cd forgefs
cargo build --locked --release        # target/release/forge
```

In a clone rustup *does* honour `rust-toolchain.toml` and will fetch the pinned Rust.

**How this page was produced.** Every command on it was executed. Unless a block says otherwise, it
ran against a `--release` build of `main` with the version-bump patch the release-preparation
workflow applies, so `forge --version` reports `forge 0.4.0`. Most transcripts were captured at commit
`4593afc`; the atomic-move, observation-epoch and graph-ceiling sections and every count on this
page were captured at `2581ae0` or later, after `mv` (#366), the I9 epoch fix (#363) and the graph
reclassification (#367) landed. The install and attestation transcripts above are runs against the **released
v0.3.0** artifacts, because v0.4.0's did not exist yet when they were captured; only `$V` differs.
Object ids for *content* are reproducible and you should see the same ones — `b6b49a01…` for
`pub fn a() {}` in both the worked example and the reclamation example below. Commit and tree ids
embed a timestamp, so those will differ on your machine, and so will every ULID.

## A worked example

Two agents edit one ref at the same time. A third reasons about a dependency that moves underneath
it. Nothing is lost, and nothing is silently accepted.

### Create a forge and put a real tree in it

```bash
mkdir -p project && printf 'fn main() { println!("hi"); }\n' > project/main.rs

forge init ./demo
export FORGE_DIR=$PWD/demo
ROOT=$FORGE_DIR/.forge/keys/root.cap

forge --cap $ROOT import --ref work ./project
forge --cap $ROOT refs
```

```text
initialized ./demo/.forge
root cap: ./demo/.forge/keys/root.cap
imported b1fceb3cca30ffce4476df58369fdb07d111ba198008f858818330af397c0697 -> work
P- commit                           main eb678424a68255d59e060221c47a7dbb2faa5e73b135fc92ab856ca195ffcf1f
-- commit                           work b1fceb3cca30ffce4476df58369fdb07d111ba198008f858818330af397c0697
```

The two flag columns are `P` protected and `S` sealed. `main` is protected at `init`: only `merge`
and `seal` may advance it, so agents never publish to it directly — and as of v0.4.0 a read-write
mount of it is refused outright rather than accepted and later denied. `import` refuses symlinks by
default; see [Limits](#limits).

### Give two agents authority, and only that

```bash
ALICE=$(forge --cap $ROOT grant --ops read,write,branch --ref 'work,heads/*,forks/*' --agent alice)
BOB=$(forge --cap $ROOT grant --ops read,write,branch --ref 'work,heads/*,forks/*' --agent bob)

forge --cap "$ALICE" grant --ops read,write --ref work --agent mallory   # mint authority?
forge --cap "$ALICE" branch work sneaky                                  # touch another ref?
```

```text
forge: denied: cap missing op grant
forge: denied: cap does not cover ref sneaky
```

Both exit 1. A capability is a compact hex token carrying `(operation, resource)` caveats — its
length tracks the caveats it holds, so `$ALICE` above is 310 bytes, `$BOB` 306, and the root cap
186. Attenuation can only shrink it, and a namespace id is not authority (I13, I14).

### Two sessions, both writing the same ref

```bash
A=$(forge --cap "$ALICE" session open --from=work)
B=$(forge --cap "$BOB"   session open --from=work)

forge --cap "$ALICE" mount --ns "$A" / ref:work --rw
forge --cap "$BOB"   mount --ns "$B" / ref:work --rw

forge --cap "$ALICE" write --ns "$A" /alice.rs --text 'pub fn a() {}'
forge --cap "$BOB"   write --ns "$B" /bob.rs   --text 'pub fn b() {}'
```

```text
mounted / -> ref:work
mounted / -> ref:work
b6b49a013ebdcf7bc69efd9d31b64bc2e458e40f77d898c63a4417f4da4bf1a3
f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc
```

Each session pinned `work` at open. Neither can see the other's staged writes, and neither can see
`work` move.

### One wins the CAS, the other forks

```bash
forge --cap "$ALICE" checkin --ns "$A" -m 'alice adds alice.rs'
forge --cap "$BOB"   checkin --ns "$B" -m 'bob adds bob.rs'
forge --cap $ROOT refs
```

```text
updated work 01a7342e30a8b944ed0cc50ec7fd7b22c442f94883e897042ad6597cc98f0de0
forked work -> heads/agents/bob/forks/work/01M0YQ0TCK7ADGPC5XY5E6V9JS ours=4604cad685a967ca767501dc49b4e04aa80b627477db2113c587efba774c9b58 theirs=01a7342e30a8b944ed0cc50ec7fd7b22c442f94883e897042ad6597cc98f0de0
-- commit                           heads/agents/alice/01M0YQ0TAQ6E2PA6NH15X0A8DA b1fceb3cca30ffce4476df58369fdb07d111ba198008f858818330af397c0697
-- commit                           heads/agents/bob/01M0YQ0TAYVMR2SZHTFSFBRRQ6 b1fceb3cca30ffce4476df58369fdb07d111ba198008f858818330af397c0697
-- commit                           heads/agents/bob/forks/work/01M0YQ0TCK7ADGPC5XY5E6V9JS 4604cad685a967ca767501dc49b4e04aa80b627477db2113c587efba774c9b58
P- commit                           main eb678424a68255d59e060221c47a7dbb2faa5e73b135fc92ab856ca195ffcf1f
-- commit                           work 01a7342e30a8b944ed0cc50ec7fd7b22c442f94883e897042ad6597cc98f0de0
```

**Both checkins exit 0.** Losing a race is a normal outcome, not an error, and the fork is a real
ref holding Bob's completed contribution (I18) — now inside Bob's own agent namespace:

```bash
FORK=$(forge --cap $ROOT refs | awk '$3 ~ /\/forks\// {print $3}' | head -1)
C=$(forge --cap "$BOB" session open --from="$FORK")
forge --cap "$BOB" ls --ns "$C" /
```

```text
blob  - f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc bob.rs
blob  - 6c32ed274accb8fc6a03f46ee042e3fb883b847395ea9b178263c217825584fc main.rs
```

Note the `awk` pattern: `/\/forks\//`, not `/^forks\//`. That is the rename.

### A read is a claim, and a stale claim is refused

Carol mounts a dependency read-only, reads one file, and then writes something entirely unrelated.
Between her read and her checkin the dependency moves.

```bash
mkdir -p vendor && echo 'API = 1' > vendor/api.txt
forge --cap $ROOT import --ref deps ./vendor
CAROL=$(forge --cap $ROOT grant --ops read,write,branch --ref 'work,deps,heads/*,forks/*' --agent carol)

D=$(forge --cap "$CAROL" session open --from=work)
forge --cap "$CAROL" mount --ns "$D" / ref:work --rw
forge --cap "$CAROL" mount --ns "$D" /vendor ref:deps
forge --cap "$CAROL" read --ns "$D" /vendor/api.txt

# meanwhile, someone else publishes deps
E=$(forge --cap $ROOT session open --from=deps)
forge --cap $ROOT mount --ns "$E" / ref:deps --rw
forge --cap $ROOT write --ns "$E" /api.txt --text 'API = 2'
forge --cap $ROOT checkin --ns "$E" -m 'api 2'

forge --cap "$CAROL" write --ns "$D" /carol.rs --text 'pub fn c() {}'
forge --cap "$CAROL" checkin --ns "$D" -m 'carol adds carol.rs'
```

```text
imported df52666880daf058e7892bdc6f9434574422708244ff69dfc6f1f740860a4645 -> deps
mounted / -> ref:work
mounted /vendor -> ref:deps
API = 1
mounted / -> ref:deps
9af68071c07115c84a52289c06bd93f4985ab058dbf490df168e054e445c088a
updated deps 6068004b8d28fcbae2e646cafe6c618990f5b64381028a2826f10bbe0a536626
07832846edcdd594e7461e79c6c15e4b2944f8b059ab68b6b057ff3f53d58220
forge: stale observation of /vendor:/api.txt: expected 0aee50505cca4e134e62f9098aa20457db4b5b857e8acb2bf83b7f652c8f62c3, found 9af68071c07115c84a52289c06bd93f4985ab058dbf490df168e054e445c088a
```

**Exit 4.** Carol's write does not collide with anything. She is refused because the file she
*reasoned from* is no longer the file she read. That is the property that makes an agent's output
trustworthy, and it is the one thing a lock-free shared checkout cannot give you. Her staged work is
not destroyed; the refusal names the mount and the path. And as of v0.4.0 it keeps holding across
checkins: the epoch of an observation is the **mount** that recorded it, so publishing one mount no
longer forgets a read taken through another (issue #329; I9 now states which epoch it means).

### Integrate, seal, verify

```bash
forge --cap $ROOT merge --into=main --from=work
forge --cap $ROOT merge --into=main --from="$FORK"
forge --cap $ROOT seal main --tag v1.0 --attest
forge --cap $ROOT verify v1.0
forge --cap $ROOT fsck --full
```

```text
merged main 0ab109fc19627b1887803ed013ee4d64bd1fff074c5ef7d34103a35b77350bc8
merged main 9d4f72fd2930e94c67cbe9f665f52cbdeabb37f61f3a1c73a5bdc90684f699c7
sealed tags/v1.0 6833fca27e1ae57e4caa60564d94b234f735c9d76bd64840c3cefe0c7dd844c4
attested ok
ok 6833fca27e1ae57e4caa60564d94b234f735c9d76bd64840c3cefe0c7dd844c4
ok (full): 10 refs, 26 objects, 5 namespaces
```

Bob's fork merged cleanly because the two agents touched different paths. Had they touched the same
path, `merge` would have exited 4 and produced a **Conflict object** under `conflicts/` holding both
immutable inputs, rather than a string in a terminal:

```text
conflict dc9019a1595fe318e148346553deb880d4507b5abc5c0ae852e3d9cd2ea0374c
forge: conflict object dc9019a1595fe318e148346553deb880d4507b5abc5c0ae852e3d9cd2ea0374c
```

```text
-- conflict                         conflicts/main/01M0YQ34JRHYWCK8YCJQSJFNP1 dc9019a1595fe318e148346553deb880d4507b5abc5c0ae852e3d9cd2ea0374c
```

## The model

Eight nouns are enough to reason about everything above.

| | |
|---|---|
| **object** | Immutable bytes. `ObjectId = BLAKE3(canonical encoding)`. Blob, Tree, Commit, Contribution, Conflict, Snapshot. Written once, never overwritten. |
| **ref** | The only mutable thing in the system. Every move is `CAS(expected → new)` — including `seal`, as of v0.4.0. `heads/`, `forks/`, `conflicts/`, `tags/` are *types*, not naming conventions. |
| **session** | `(capability, namespace, pin, observations)`. Opening one pins a commit and gives you a `/` mount plus a `/main` mount. |
| **mount** | A path in the session bound to a spec. `ref:NAME --rw` makes that ref the publication target, and a spec that could never be published is refused here rather than at checkin. Every read-write mount carries **its own** pinned base, so reads never come from a live ref another agent can move. |
| **observation** | Every read records `path → what it saw`: a blob id, a tree id, or *absence*. Absence counts; silence does not. |
| **contribution** | A checkin is a typed Contribution object bound to the agent, not a log message. |
| **capability** | `(operation, resource)`. Attenuation only shrinks. There is no ambient root. |
| **seal** | An ed25519 signature over a snapshot. `verify` re-reads durable bytes and walks the typed graph against *this forge's* trusted key. |

One publication, exactly:

```text
write, mv        -> stage into the mount's overlay
checkin --mount  -> resolve that mount BY NAME, not by path prefix
                 -> fold that overlay onto that mount's pin
                 -> build a Contribution
                 -> re-check every observation
                 -> CAS the ref that mount names, from that pin

  CAS won                       exit 0   updated <ref> <oid>
  CAS lost                      exit 0   forked <ref> -> heads/agents/<agent>/forks/... (work preserved)
  an observation moved          exit 4   stale observation of <mount>:<path>
  same path both sides at merge exit 4   Conflict object under conflicts/
  --mount names no mount        exit 1   lists the mounts this session actually has
  nothing to publish, but the
  session holds staged work     exit 1   names the mounts that still hold it
```

Automation keys on those exit codes, never on stderr wording: `4` is a stale observation, a merge
conflict or a moved head under `seal`; `2` is corruption or a sealed-state violation; `1` is denial
or bad input; `3` is transient contention; `5` is I/O or internal failure.
[`CLI_ABI.md`](CLI_ABI.md) is the contract; [`scripts/cli-abi-conformance.sh`](scripts/cli-abi-conformance.sh)
executes it as **57 rows, 54 of them blocking** and three declared *unexercised* rather than faked.

## What changed in v0.4.0, verb by verb

Every transcript in this section was produced by the v0.4.0 build. Where a v0.3.0 behaviour is
quoted, it comes from the issue that recorded it.

### `mount --rw` refuses at mount time what checkin could never publish

Three read-write mount specs can never be published, and all three are now refused when you ask for
the mount, not after you have staged work behind it:

```bash
forge --cap $ROOT mount --ns "$S" / ref:main --rw          # main is protected
forge --cap $ROOT mount --ns "$S" / oid:$OID --rw          # an oid names bytes, not a ref
forge --cap $ROOT mount --ns "$Z" / "ref:$CONF" --rw       # a ref holding a Conflict, not a commit
```

```text
forge: denied: cannot mount ref:main read-write at /: ref main is protected, so session checkin can never advance it and a write through this mount could never be published; mount it read-only, or branch it and mount the branch
forge: denied: cannot mount oid:e79e12dd88b0a466a81360c9255c63dd671dee56ec3672e5cd6bd0efc4ce1086 read-write at /: an oid: spec names immutable bytes with no ref to advance, so a write through it could never be published; mount it read-only, or mount the ref that carries it
forge: denied: cannot mount ref:conflicts/main/01M0YQ34JRHYWCK8YCJQSJFNP1 read-write at /: it names a conflict, and only a commit ref can be advanced by checkin
```

All three exit **1**. Read-only mounts of the same specs are still fine and still succeed:

```text
mounted /snap -> oid:e79e12dd88b0a466a81360c9255c63dd671dee56ec3672e5cd6bd0efc4ce1086
mounted /c -> ref:conflicts/main/01M0YQ34JRHYWCK8YCJQSJFNP1
```

In v0.3.0 the protected case was accepted, the `write` was accepted, and only `checkin` denied — at
which point `abandon session` refused too, because work was staged, and the only exit was
`abandon session --discard-staged`, which throws the work away. That wedge is what issue #328 was.
The property behind the fix is stronger than the three cases: `every_rw_mount_spec_is_refused_or_publishable`
enumerates seven rw specs, each minted by the verb that really produces it, and asserts each is
either refused at mount time or publishable — where publishable means write → `checkin --mount` →
`abandon` with no discard.

### `checkin --mount` names a mount, not a path

```bash
forge --cap $ROOT checkin --ns "$Z" --mount /this-mount-does-not-exist -m typo
```

```text
forge: not found: session 01M0YQ1ZS76VW7RN55FS61A9D3 has no mount at /this-mount-does-not-exist; checkin folds exactly the mount it is given and publishes no other, so it will not fall back to a default. This session mounts: /, /main, /w1
```

Exit **1**, and nothing is published. In v0.3.0 the argument was resolved with the helper that finds
the mount *containing a path*; since every session has a `/` mount and `/` prefixes everything, a
typo silently published the default mount and answered `updated`, exit 0 — including answering
`noop`, the one outcome `CLI_ABI.md` says callers may rely on, for a mount that does not exist
(issue #353). A path *inside* a mount is not that mount; only the exact name resolves, modulo
leading and trailing slashes.

### `mv` — a move is one staged transaction, not copy plus delete

New verb, and a new invariant (I24). `forge mv --ns <ns> <from> <to>` supersedes the source and
destination subtrees, stages every destination row and writes the source tombstone in **one catalog
transaction**, so the only observable states are before and after: nothing sees the content at both
paths, and nothing sees it at neither.

```bash
forge --cap $ROOT mount --ns "$S" / ref:work --rw
forge --cap $ROOT mv --ns "$S" /old /new
forge --cap $ROOT ls --ns "$S" /
forge --cap $ROOT checkin --ns "$S" -m 'move old to new'
```

```text
mounted / -> ref:work
moved /old /new tree 2447c9defee8626dd0400533fc6e24ab43300a0815273eb9496cf15185621357 entries=1
blob  - 9c77e92db88530dda32ae0af5e0b228ffc6721eee1c9922e8fc5acf52c9e6415 main.rs
tree  - 0000000000000000000000000000000000000000000000000000000000000000 new
updated work 6c4d77b390377594efc6f4c248a2e4a2f3b68a34dee69cd3a25a688968d4c567
```

It adds no commit point: publication is still one overlay fold, one Contribution, one CAS. It also
**never spans mounts**, because two mounts pin two refs (I19) and publish separately, so there is no
transaction that could carry both halves:

```text
forge: invalid: rename crosses mounts: /new/a.rs resolves through / and /w2/a.rs through /w2; each mount pins its own ref and publishes separately (I19), so there is no transaction that could carry both halves
```

Exit **1**, refused rather than half-applied. The zero tree id on `new` in the listing above is the
staged-directory display noted under [Limits](#limits); in a session opened after the checkin the
same entry reads `2447c9de…`, the tree the move reported.

### `seal` compare-and-swaps the ref it names

`seal` reads a ref, builds a snapshot from it, and publishes a tag. In v0.3.0 those were two
observations: if the head moved in between, the tag was published anyway and named the *pre-race*
commit, `verify` passed, `fsck --full` passed, and the provenance claim — the entire point of a seal
— was quietly false (issue #331).

In v0.4.0 the ref is compared against the oid the snapshot was built from **inside** the seal
transaction. A moved head is `StaleObservation`, exit **4**, and nothing is published: no tag ref,
no reflog entry, no seals row, and the seal is never silently retargeted at the new head. Exit 4
rather than 1 because the request was well formed and authorised, and re-issuing it succeeds.

The window is a race, so it is driven deterministically by a test barrier rather than by a shell
transcript, and the ABI row for it is declared `unexercised` in the conformance table instead of
being faked:

```bash
cargo test -p forge-cli --test cli_seal_head_moves
```

```text
running 1 test
test cli_seal_refuses_when_the_ref_moves_inside_the_seal_window ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.23s
```

```text
unexercised abi/4-seal-moved-head | needs a second process to move the ref between seal's read and
its publish; driven deterministically by crates/forge-cli/tests/cli_seal_head_moves.rs via
FORGEFS_TEST_SEAL_CAS_BARRIER, which exists only in a debug build
```

### `serve` is a specified projection of the CLI — a wire-format change

The daemon serves **9 ops**, every one of them a CLI verb, against the CLI's 26 verbs. It is a
strict subset by design and now by documentation. What changed in v0.4.0 is that it is also
*specified*: `DAEMON_OPS` declares every op and every field each op accepts, and anything else is
refused before the op runs.

Started with `forge --cap $ROOT serve --http`, which adds a loopback listener on `127.0.0.1:4077`;
the capability goes in an `Authorization: Bearer` header:

```bash
curl -sS -X POST http://127.0.0.1:4077/v1/ns.frobnicate \
  -H "Authorization: Bearer $CAP" -H 'content-type: application/json' -d '{}'
```

```text
HTTP 400
{"v":1,"id":1,"ok":false,"err":{"code":"invalid","msg":"invalid: unknown op ns.frobnicate"}}
```

```bash
curl ... /v1/ns.checkin -d '{"ns":"...","mnt":"/typo"}'
```

```text
HTTP 400
{"v":1,"id":1,"ok":false,"err":{"code":"invalid","msg":"invalid: unknown field \"mnt\" for op ns.checkin; accepted: ns, mount, msg"}}
```

```bash
curl ... /v1/ns.mount -d '{"ns":"...","path":"/","spec":"ref:work","rw":"true"}'
```

```text
HTTP 400
{"v":1,"id":1,"ok":false,"err":{"code":"invalid","msg":"invalid: rw must be a boolean"}}
```

In v0.3.0 all three of those were accepted: an unknown field was a silent default, so
`{"mnt": "/typo"}` published the *default* mount, and `"rw":"true"` — a string, not a boolean —
mounted **read-only**. A CLI user got exit 1 for each.

**The wire-format change.** `ns.checkin` used to answer with a Rust `Debug` rendering of an internal
enum:

```json
{"v":1,"id":1,"ok":true,"body":"Updated { name: \"work\", oid: ObjectId(528cb430…) }"}
```

It now answers a structured object:

```json
{"v":1,"id":1,"ok":true,"body":{"name":"work","oid":"2f479e65c35374ec68184dba3d3225a49afbde277fc70eb5f501aaa9ec75dfa5","result":"updated"}}
```

Nothing documented or tested the old shape, but a client that parsed it breaks. Error classification
now comes from the same `Error::exit_code()` table the CLI uses, and the same refusals appear:

```text
HTTP 404
{"v":1,"id":1,"ok":false,"err":{"code":"not_found","msg":"not found: session 01M0YQAVBY6TAQ0CTYJMYYAW36 has no mount at /typo; checkin folds exactly the mount it is given and publishes no other, so it will not fall back to a default. This session mounts: /, /main"}}

HTTP 403
{"v":1,"id":1,"ok":false,"err":{"code":"denied","msg":"denied: cannot mount ref:main read-write at /m: ref main is protected, so session checkin can never advance it and a write through this mount could never be published; mount it read-only, or branch it and mount the branch"}}
```

HTTP status is lossier than the exit-code table on purpose — 409 covers `sealed`, `conflict`,
`stale_observation` and `invalid_base` alike — so a client that needs the CLI's classification reads
`err.code`, never the status.

### Oversized caller input is `invalid`, not `corrupt`

A tree may hold at most 100,000 entries. In v0.3.0 that limit was enforced only on the way *back
out*, in `Tree::decode`, so `forge import` of an ordinary directory with more entries walked it,
stored every blob in it, and then reported `forge: corrupt: tree fanout exceeds limit`, exit 2 — the
code reserved for corruption — about an intact source directory and a brand-new repository
(issue #355).

```bash
ls wide2 | wc -l
forge --cap $ROOT import --ref wide2 ./wide2
```

```text
100001
forge: invalid: import refuses /workspace/w/big/wide2: it holds 100001 entries, more than the 100000 a tree may hold; split it into subdirectories
```

Exit **1**, in one second, and the object count before and after the refusal was the same (5 and 5):
the check runs on the dirents before a single blob of the doomed directory is read. One entry fewer
imports normally. The same reclassification covers the overlay fold — which names the *directory*
the staged writes landed in — and `Conflict` objects, where a merge over more than the conflict-item
limit used to write an object that `Conflict::decode` then rejected as corrupt.

What did **not** move: a tree or conflict object read back **from the store** over the limit is
still exit **2**. No encoder in this binary can produce those bytes, so finding them in an object
file really is damage, and collapsing that into exit 1 would destroy a real corruption signal.

### `fsck` and `gc` say true things

`fsck` on an un-migrated catalog is covered under [Upgrading](#upgrading-a-v021-repository) above.
The `gc` corrections are in [Reclamation](#reclamation) below. Both follow the same rule as
issues #355 and #348: the repository's age, the caller's input, and a missing tool are not
corruption.

## Why you should believe any of this

[INVARIANTS.md](INVARIANTS.md) is 24 numbered rules, I1 through I24. That file is not a manifesto,
and this is the part that is genuinely unusual:

**Every rule names its production owner and its test.** Under the "Executable evidence" heading is
a table mapping every one of I1–I24 to the module that implements it and to the exact test files
that prove it. I18 ("a refused checkin never destroys staged work") points at
`forge-api/workspace.rs`, `forge-api/gc.rs`, `forge-store/meta.rs`, and at
`pinned_rw_session_reads.rs`, `cli_shared_stampede.rs`, `gc_and_abandon.rs`, `model_composition.rs`
and `docs/GC.md`. You can check any claim on this page by opening the row.

**A PR that cannot name an invariant does not merge.** That is a stated rule in INVARIANTS.md,
enforced by review rather than by CI. When a fix needs a rule that does not exist yet, the rule gets
added: I19–I21 arrived with the multi-mount pinning fix, I22 with the checkin refusal, I23 with
garbage collection, and I24 with the atomic move documented above.

**The evidence was itself audited by mutation.** A 45-mutation audit (#301) applied, one at a time,
the smallest production edit that genuinely violates each invariant, and re-ran the full suite plus
both gate scripts against each. Result: **36 caught, 9 not**. The sharpest miss was I15 — deleting
the seal signature check outright left the entire suite green. Each of the nine gaps was closed with
a test verified to fail with the mutation applied and pass with the source restored, both directions
reported. `verify_rejects_seal_signature_not_made_by_this_forge_trusted_key` is that test. This was
a one-time audit, not a recurring CI job.

**Three shapes of evidence, kept deliberately separate.** Deterministic property tests state an
invariant as algebra and drive it from a seeded generator, with no property-testing dependency and
the seed printed on failure (`property_canonical.rs`, `property_merge_symmetry.rs`,
`property_attenuation.rs`). Six fuzz targets feed the same boundaries untrusted bytes —
`object_decode`, `cap_token`, `protocol_frame`, `tree_name`, `ref_name`, `tar_roundtrip` — and CI
compiles every one and smokes each for 60 seconds. Real race, cross-process, `SIGKILL` and
filesystem harnesses are kept out of the shared table test so that a real boundary is not diluted
into a mock.

**Composition is tested against a model, not just operation by operation.**
`crates/forge-api/tests/model_composition.rs` keeps a naive, obviously-correct in-memory model —
refs to flat path/byte maps, a per-mount pinned base, a per-mount staged overlay, the observation
set — drives seeded random sequences of `session open / mount / write / delete / read / ls /
checkin / branch / abandon / seal` against a real `Forge`, and after *every* step asserts that the
model and the repository agree. It exists because #326 was a composition bug: `mount`, `write` and
`checkin` were each correct and the composition silently lost the write, so no per-operation test
could see it. Its `KNOWN` table of characterised defects is asserted to be *fully observed*, so
fixing one of them fails the test until the row is deleted — an allow-list that cannot rot. **As of
v0.4.0 `KNOWN` is empty** (`const KNOWN: &[(&str, &str)] = &[];`): the liveness checks that used to
file a wedge now fail the run instead, and the two wedges they were filing — a read-write mount of a
protected ref, and a read-back of a session's own staged write that made every *other* mount's
checkin permanently stale — are fixed.

**The release verifies itself, and you can re-run that.** `scripts/release-gate.sh` drives a fresh
repository through the whole contract with a packaged binary — same-path overlap producing a
Conflict object, a stale-observation refusal, seal + attest + verify, `fsck --full`, and the full
CLI ABI table — and it runs on all four targets during the release, from the packaged binary rather
than a rebuild. Its evidence is published beside the tarballs and covered by `SHA256SUMS`.

### The gates need nothing but bash, coreutils, sed and awk

That sentence used to be a claim. Twice it was false, and each time the falsehood took the same
shape: a **missing tool reported as a failing product**.

`python3` was required for JSON shaping alone, and its absence exited **2** — the code
[`CLI_ABI.md`](CLI_ABI.md) reserves for corruption — on a base Debian image (issue #346). JSON is
shaped by `scripts/json-lib.sh` now and the dependency is gone. Then `grep` — which is its own
Debian package, not part of coreutils — turned up in one `grep -Eq` in the middle of the gate, and
on exactly the declared PATH produced `gate: FAIL gate/conflict-object` and `"ok": false` in
`gate-summary.json` (issue #354). The match is done in awk now, so that dependency is *gone* rather
than merely declared.

Fixing the one `grep` fixes neither class, so v0.4.0 adds [`scripts/prereq-lib.sh`](scripts/prereq-lib.sh),
which makes the declaration executable in both directions:

- **The declared list.** `GATE_REQUIRED_COMMANDS` names every external command the gates may run —
  currently `awk basename bash cat chmod date dirname env head mkdir mktemp od rm sed seq sort tr
  uname wc`. `require_declared_commands` runs before the first assertion. Measured: build a PATH
  holding symlinks to exactly that list minus `od`, and run the gate.

  ```text
  release-gate: harness error: missing prerequisite command(s): od
  release-gate: prerequisites, in full: awk basename bash cat chmod date dirname env head mkdir mktemp od rm sed seq sort tr uname wc
  ```

  Exit **3** — "the harness could not run" — not 1, because no assertion about forge was disproved,
  and not 2, because nothing is corrupt. Put `od` back and the same PATH runs the gate to
  `release-gate: PASS`, exit 0. That is the check, not a claim: the declaration is enforced by
  running both gates on a PATH built from it and nothing else
  (`crates/forge-cli/tests/gate_scripts_need_no_interpreter.rs`).

  One caveat, measured: the up-front refusal happens before the output directory is touched, so a
  `gate-summary.json` left by an *earlier* run is still sitting there afterwards. It is the earlier
  run's, and its `started_unix` says so.

- **The backstop, where the shell has one.** An *undeclared* command that some later diff reaches
  for anyway is caught by `command_not_found_handle` plus `prereq_guard`, which convert the verdict
  into exit 3 as well. **This half is not portable, and this page will not pretend otherwise.**
  `command_not_found_handle` arrived in bash 4.0; macOS ships bash 3.2.57. Measured on this Mac's
  `/bin/bash`:

  ```text
  bash 3.2.57(1)-release
  undeclared-command backstop: absent
  command_not_found_handle: not defined
  ```

  The declared-list check *does* work there — the same probe under bash 3.2.57 on a PATH with
  nothing on it exits **3** and names every missing command, because it is built from `command -v`,
  `for` and `[`, all builtins. What degrades on bash 3.2 is only the undeclared case: the shell
  still names the tool on stderr itself, but the run exits **1** with a `gate: FAIL` row. There is
  no POSIX substitute — an `ERR` trap does not fire in a condition context, and `if ! cmd` is
  exactly the shape the `grep` defect had. `prereq_backstop_available` reports which of the two
  worlds a run is in, and the tests assert each mechanism only where it exists.

### Run the gates yourself

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
bash scripts/cli-abi-conformance.sh target/release/forge
bash scripts/release-gate.sh target/release/forge
```

At the v0.4.0 tree, on the box described under [What it costs](#what-it-costs), those produce:
`fmt` clean; `clippy` clean; **431 tests passed, 0 failed**, 2 ignored, across 114 test binaries;

```text
abi rows=57 blocking=54 known_failing=0 unexercised=3 blocking_failures=0
abi-conformance: CLI_ABI.md contract holds for every blocking row
```

```text
release-gate: PASS - forge 0.4.0 sealed and verified itself as v0.4.0
release-gate: artifacts in /workspace/forgefs/release-gate-out
```

The three unexercised rows are declared, with the reason and the test that does cover them:

```text
abi/1-fsck-unmigrated-catalog  needs a catalog at an older metadata schema version
abi/3-busy                     needs a second process holding a SQLite write txn past busy_timeout
abi/4-seal-moved-head          needs a second process to move the ref between seal's read and its publish
```

The first of those is exercisable by hand in about a minute if you want it: build a repository with
the released v0.2.1 binary and point a v0.4.0 `fsck --full` at it, which is the transcript under
[Upgrading](#upgrading-a-v021-repository).

`release-gate-out/gate-summary.json` records what each phase actually proved. Four of the
`phases_passed` entries, `id` and `detail`:

```text
gate/same-path-overlap
  merge exit 4, Conflict 4943eec53b93283a8a2c90524d1210808041cccb4f95c0106c9b70e2bcc871d2 at conflicts/main/01M0YQHEXN20S9G2E12X8T7JQC, main pinned at 56be596d3fbb76f6f717b096c7ad56dabf6c6957d287359bd923414f6a27812c
gate/stale-observation
  checkin exit 4, heads/agents/bob/01M0YQHF0PEGVTQW57D2TM04VD pinned at '9e22480a35041f9178a4ba5b1043e46ce5dcac7600f3318a314b4ed561e79a76', control ref advanced to da7d1128b1a50c1524fa7c9092a786de332993fbc0ca97e00182a60da331feac
gate/seal-verify
  sealed tags/v0.4.0 -> 79b694b39ea7ea343ead64e31dc0619a41df5fbe54b7266bc7ce55c9edcba397, --attest ok, verify ok, flags PS
gate/fsck
  fsck --full --json ok: refs=13 objects=40 namespaces=10 findings=0
```

The gate is versioned with the contract, not with the binary, so it is also a skew detector: run the
current `main` gate against an older shipped tarball and it correctly fails the rows for behaviour
that postdates it. Use the gate evidence published with a release to check that release, and the
in-tree gate to check the tree.

## Reclamation

A fork is a GC root until it is explicitly resolved — merged, or retired with `abandon`. That is
what keeps I18's promise from turning into unbounded growth. `gc --collect` then reclaims for real.
A self-contained example, in its own forge, with one losing writer to produce actual garbage:

```bash
forge init ./gcdemo
export FORGE_DIR=$PWD/gcdemo
ROOT=$FORGE_DIR/.forge/keys/root.cap
mkdir -p src && echo 'fn main() {}' > src/main.rs
forge --cap $ROOT import --ref work ./src

A=$(forge --cap $ROOT session open --from=work); forge --cap $ROOT mount --ns "$A" / ref:work --rw
B=$(forge --cap $ROOT session open --from=work); forge --cap $ROOT mount --ns "$B" / ref:work --rw
forge --cap $ROOT write --ns "$A" /a.rs --text 'pub fn a() {}'
forge --cap $ROOT write --ns "$B" /b.rs --text 'pub fn b() {}'
forge --cap $ROOT checkin --ns "$A" -m a
forge --cap $ROOT checkin --ns "$B" -m b

FORK=$(forge --cap $ROOT refs | awk '$3 ~ /\/forks\// {print $3}' | head -1)
forge --cap $ROOT abandon session "$A" --discard-staged
forge --cap $ROOT abandon session "$B" --discard-staged
forge --cap $ROOT abandon fork "$FORK"
```

```text
initialized ./gcdemo/.forge
root cap: ./gcdemo/.forge/keys/root.cap
imported a377f822ecf8f8a93498f50902c06c44b23bc27b629c5e275b990c5ca65d2bd9 -> work
mounted / -> ref:work
mounted / -> ref:work
b6b49a013ebdcf7bc69efd9d31b64bc2e458e40f77d898c63a4417f4da4bf1a3
f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc
updated work 7ba93430ed18956f62b7c025211db27ceb999998d13c09559721e175236c6c52
forked work -> heads/agents/anon/forks/work/01M0YQ5F05EFVRE66D2WVRGVMN ours=bc1039ce45468dc9f1c8f4c75f0500a73f1e10685f2fde5806ba808a3b4b6185 theirs=7ba93430ed18956f62b7c025211db27ceb999998d13c09559721e175236c6c52
abandoned session 01M0YQ5EYF3BNDZCJ4NEWQZW7W discarded=0 mounts=2 observations=0
abandoned session 01M0YQ5EYS095EQZZ6GWFJXK59 discarded=0 mounts=2 observations=0
abandoned heads/agents/anon/forks/work/01M0YQ5F05EFVRE66D2WVRGVMN commit bc1039ce45468dc9f1c8f4c75f0500a73f1e10685f2fde5806ba808a3b4b6185
```

`gc` needs a mode, and says which two it has:

```bash
forge --cap $ROOT gc
```

```text
forge: invalid: gc needs a mode: --dry-run to plan, or --collect to reclaim (see docs/GC.md)
```

Exit 1. In v0.3.0 that same exit-1 answer was `gc supports --dry-run only; collection is not
implemented (see docs/GC.md)` — left over from before collection existed, still printed by a binary
whose `gc --help` listed `--collect` and whose `CLI_ABI.md` documented it. The exit code was right
the whole time, which is exactly why the conformance suite could not catch it: it checks status
codes, not sentences (issue #356).

Now plan, then collect. There are 13 object files:

```bash
forge --cap $ROOT gc --dry-run --min-age-secs 60
sleep 65                       # the new garbage has to age past --min-age-secs first
forge --cap $ROOT gc --collect --min-age-secs 60
find $FORGE_DIR/.forge/objects -type f | wc -l
forge --cap $ROOT fsck --full
```

Before the sleep, the plan withholds everything:

```text
gc (dry-run, min-age 60s): 9 of 13 objects reachable
roots: 4 refs (0 unresolved forks), 0 session pins, 0 live refs, 0 mounts, 0 mount pins, 0 overlay blobs, 0 observations, 1 landmarks, 0 seals
collectable: 0 objects, 0 bytes
withheld (younger than min-age): 4 objects, 473 bytes
nothing was deleted; this was a dry run
```

After it, the same plan is taken:

```text
gc (collect, min-age 60s): 9 of 13 objects reachable
roots: 4 refs (0 unresolved forks), 0 session pins, 0 live refs, 0 mounts, 0 mount pins, 0 overlay blobs, 0 observations, 1 landmarks, 0 seals
collectable: 4 objects, 473 bytes
withheld (younger than min-age): 0 objects, 0 bytes
collectable 2fd413b69a9bd0de227d4b9f036c2be58837ca6a28603bcecac36e4abae469d1
collectable 7976b43e084d158f90feb52c29449a1ac4ddba0f82afd01e92536d53b9d34254
collectable bc1039ce45468dc9f1c8f4c75f0500a73f1e10685f2fde5806ba808a3b4b6185
collectable f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc
collected: 4 objects unlinked
9
ok (full): 4 refs, 9 objects, 0 namespaces
```

**`--dry-run` and `--collect` now agree.** Both say `9 of 13 objects reachable`, on the same
repository with nothing changed between the runs. In v0.3.0 the sweep reported `16 of 16 reachable`
beside `withheld: 2` — a contradiction, because `--collect` deliberately widens its reachable set
into a *protection* set so that nothing a withheld survivor names is unlinked, and then assigned the
widened size to the reported reachable count. Nothing was ever wrongly deleted; the operator was
simply told, on the one path that deletes, that there was nothing to delete. Both halves compute
reachability through one shared closure now (issue #356).

Two separate rules are at work in that `sleep`. `--min-age-secs` is a hard floor, not a hint:

```text
forge: invalid: gc --collect requires --min-age-secs >= 60: the floor bounds the window in which a writer has put an object and not yet published a root naming it, and a floor below it collects live data (see docs/GC.md)
```

because an object is fsynced before the catalog row that roots it (I4), so a lower floor can collect
live data. And an object younger than the floor is *withheld* rather than taken — the `--collect`
above without the `sleep` exits 0 having deleted nothing, reporting `collected: 0 objects unlinked`.
The sleep is for the second rule, not the first.

`abandon fork` refuses a ref that is not a fork and refuses a live session head — both are rows in
the ABI conformance table — and `abandon session` refuses a session holding staged work unless you
pass `--discard-staged`. `--dry-run` computes the same plan and deletes nothing.
[`docs/GC.md`](docs/GC.md) has the root set and the sweep-race argument.

## What it costs

Numbers below are from `forge bench`, five fresh repositories per configuration, medians, on one
box. They are a regression signal for *this* hardware, not a "fastest filesystem" claim.

Environment, emitted verbatim by `scripts/forge-env-line.sh`:

```text
forgefs commit:        4593afcfeee8502607cd29b7e1ff081c6bb41471
build profile:         release
forge --version:       forge 0.4.0
rustc:                 rustc 1.98.0 (88d9e12ae 2026-08-18)
command line:          forge bench --agents 32 --shared 16 --workers W --scratch <fresh under /workspace>
worker count:          4, 16, 64
cpu model:             AMD EPYC 9454 48-Core Processor
cpu logical cores:     4
ram:                   7.8 GiB (8343212032 bytes)
os:                    Debian GNU/Linux 12
kernel:                Linux 6.16.9+
arch:                  x86_64
filesystem:            ext2/ext3
storage device:        /dev/vdd[/workspace]
sqlite journal_mode:   WAL
sqlite synchronous:    FULL (declared; Meta::open fails closed without it, docs/RECOVERY.md)
macos fullfsync:       n/a (Linux fsync path, docs/RECOVERY.md)
run class:             cold
repetition:            1..5 (fresh repository per invocation)
repository class:      /workspace/w/bench/envrepo (fresh repository per invocation)
```

Two readings of that block that the script is honest enough not to guess at: `filesystem:
ext2/ext3` is what its superblock-magic classifier can distinguish, and `/proc/mounts` says the
filesystem is `ext4`; `rustc` is whatever `rustc` was on `PATH` when the line was emitted, and the
`--release` binary itself was built by the toolchain `rust-toolchain.toml` pins.

**Barrier reach was established first, and it matters more than any number here.** The sandbox
mounted `/workspace` `nobarrier`, under which no durability measurement means anything. After
`sudo mount -o remount,barrier /workspace`, 200 `fsync` calls moved the `flush requests` counter in
`/proc/diskstats` for `vdd` from 202 to 402 — exactly 200, ratio 1.00. Every figure below was taken
after that, on `/dev/vdd`, not on the overlay `/tmp`. **If you re-run this and do not check that
first, you are measuring nothing.**

`forge bench` refuses `--dir`/`FORGE_DIR` outright (`forge: invalid: bench does not accept
--dir/FORGE_DIR; use --scratch <new-path> or omit it`, exit 1), so if you exported `FORGE_DIR` for
the worked example above, unset it first. One run looks like:

```bash
unset FORGE_DIR
forge --cap ./demo/.forge/keys/root.cap bench --agents 32 --shared 16 --workers 16 --scratch ./bench-run
```

`forge bench --agents 32 --shared 16 --workers W`, median of 5:

| W | private checkin ops/s | p50 | p99 | shared stampede p50 | device flushes / run | SQLite `txn_count` / run |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 255.6 | 14.96 ms | 18.41 ms | 6.98 ms | 1378 | 239 |
| 16 | 339.3 | 42.49 ms | 57.68 ms | 30.21 ms | 1107 | 188 |
| 64 | 286.8 | 102.21 ms | 109.02 ms | 35.45 ms | 1095 | 176 |

Device flushes are the `/proc/diskstats` flush-request delta across each run. `txn_count` is a
**process-lifetime** counter spanning init, both workloads, merge/seal, verify and `fsck` — it is
not a per-checkin number and must never be divided by one. It is in the table because every run does
exactly the same work, so the whole-run totals are comparable across `W` and nothing else here is;
treat it as a diagnostic that shows *direction*, never as a measurement of an operation. Serial checkin — one agent at a time, true operation latency — had p50
between **6.45 ms and 19.35 ms** across the fifteen runs. The shared stampede returned
`updated=1 forked=15` in all fifteen, and every run ended `busy=0 denied=0 stale=0 conflict=0`,
with lifetime `fsync_file=351` and `fsync_dir` between 892 and 896 regardless of worker count.

**Do not over-read the absolute numbers.** This is a shared virtual machine. Within this single
campaign the five W=16 repetitions ran 257.6, 309.0, 339.3, 354.1 and 362.3 ops/s — a 1.4x spread
inside one configuration, comparable to the effects being discussed, and earlier campaigns of this
same protocol on this class of box have put W=16 anywhere from 356 to 526 ops/s. What is stable
across all of them is the *shape*: the SQLite commit count falls with concurrency, the device-flush
count falls with it, p50 rises monotonically, and the serial floor sits in single-digit
milliseconds.

Three honest readings.

**The floor is a durability barrier.** One uncontended checkin is `write → fsync(file) → exclusive
publish → fsync(parent directory)`. Lifetime counters for the run are `fsync_file=351` and
`fsync_dir≈893` — **directory** barriers outnumber file barriers 2.5 to 1, and that is where the
cost sits.

**Group-committing the SQLite catalog is a real win, and the counters move the way it predicts.**
As concurrency rises the whole-run `txn_count` falls 239 → 188 → 176 and total device flushes fall
1378 → 1095, because N waiting writers now share one WAL fsync (`synchronous=FULL` untouched). Those
are lifetime totals for identical runs, so they establish the direction, not the size, of the
effect. The change that introduced it measured **+33% throughput at 16
concurrent writers**, 2.82x fewer durable commits, and mutex wait down from 31.6 ms to 9.5 ms, on
the machine where it landed. It is absent at W=1 and W=2, exactly as the mechanism predicts;
[`docs/BENCH.md`](docs/BENCH.md) explains why membership in a shared fsync is structural rather than
statistical. That +33% is quoted from the run that landed the change, not re-measured here — there
is no v0.4.0 build without group commit to compare against.

**Fewer `fsync` calls does not mean faster.** The counters above make the point on their own:
`fsync_dir` is ~893 per run at every worker count, yet actual device flushes fall from 1378 to 1095
and throughput rises. The barriers were already overlapping. The obvious next optimisation —
collapsing the nine directory barriers of an object publication down to two with I4 intact — has
been measured under issue #177 and **lost 15–22% at W=2..16**: a barrier that follows other barriers
costs about 49 µs against a 402 µs average, and jbd2 already merges concurrent fsyncs, so collapsing
removes the cheap barriers and gives up the overlap. The collapse is implemented and selectable as
`FORGEFS_DIR_BARRIER=collapsed`, with a middle `deferred` setting between the two, and
`per-directory` stays the shipped default precisely because collapsing is not a speedup. It is
recorded here because it contradicts the naive model, not because the default moved. **Count device
flushes, not `fsync` calls** — and if you quote the 15–22%, quote the mechanism with it, because on
a device or a journal that does not merge concurrent flushes the sign could differ.

**Throughput and latency do not move together.** Going from 4 to 16 workers on 4 logical cores buys
33% more throughput and costs 2.8x the p50; going on to 64 costs another 2.4x the p50 and *loses*
throughput. If you care about tail latency, oversubscribing the CPU is the first thing to stop
doing.

**ForgeFS versus Git, stated against itself.** The checked-in comparator
(`scripts/w7-git-comparator.sh`, protocol in [`docs/BENCH.md`](docs/BENCH.md)) was re-run on the
same box, 32 agents, 4 workers, 9 fresh repositories per configuration. Every row is on one
filesystem (`/dev/vdd`, ext4), and 200 `fsync`s on it moved the device flush count by 200 before
any figure below was taken:

| Configuration | ops/s | p50 | p99 |
|---|---:|---:|---:|
| ForgeFS, in-process threads (`forge bench`) | 245.8 | 15.08 ms | 20.61 ms |
| **ForgeFS through the `forge` CLI, 3 execs/agent** | **131.4** | 27.12 ms | 57.97 ms |
| **Git worktrees, as shipped** | **344.9** | 10.65 ms | 18.10 ms |
| **Git worktrees, `core.fsync=all core.fsyncMethod=fsync`** | **254.7** | 14.06 ms | 23.34 ms |

**ForgeFS is slower**, against both Git configurations, on the row that describes how an
orchestrator actually drives either tool — and it is slower in the in-process row too, which
earlier tables did not show. Those tables ran `forge bench` on `$TMPDIR` while the Git worktrees
sat on the repository disk; on this box that alone was a 3.3x difference, and it flattered exactly
the one row ForgeFS used to win. The comparator now places all four configurations on one
filesystem and publishes the barrier-reach probe with the numbers;
[`docs/BENCH.md`](docs/BENCH.md) carries the correction and the paired measurement.

Read the deficit as lower raw throughput under this protocol rather than as a like-for-like defeat,
because ForgeFS is doing strictly more durability work — measured, not asserted, by
`scripts/w7_fsync_probe.c` for one agent operation:

| Path | file fsync | dir fsync |
|---|---:|---:|
| ForgeFS write+checkin | 6 | 20 |
| Git add+commit, default | 0 | 0 |
| Git add+commit, `core.fsync=all` | 6 | 0 |

so the script's own durability gate marks both Git rows **`non-comparable: durability
mismatch/unknown`**, and **neither quotient is quoted here as a speed ratio**. Some of the gap is
not storage at all: the same driver measured a bare `git rev-parse --verify HEAD` per agent at
1176.1 ops/s, and each Git agent operation costs two more execs than that, so any deficit smaller
than that floor is process-model cost. Correctness was gated on both sides — ForgeFS `fsck --full`,
Git `fsck --strict` plus a per-agent check that every branch carries exactly one new commit with the
exact bytes — and all runs passed. `git version 2.39.5`.

Two things `forge bench` deliberately does **not** report: object-byte accumulation is
uninstrumented and prints the literal `bytes=unavailable`, and the per-checkin cost mix (`hash +
encode + fsync_file + fsync_dir + sqlite_wait + sqlite_txn`) prints `per-checkin mix = unavailable;
requires operation-scoped tracing; never derive it from lifetime totals`. `forge stats --json`
carries no byte field at all, and its `note` says the same thing in prose. In both commands the
counters are cumulative process-lifetime totals spanning init, both workloads, merge/seal, verify
and `fsck`, and must not be divided by a checkin count. [`docs/BENCH.md`](docs/BENCH.md) owns the
protocol.

## Limits

Each of these was re-checked against the v0.4.0 binary, or against the repository settings, before
being written down. The previous list is not reproduced unchanged: two entries were **removed
because they are no longer true**, and the ones that changed shape say what is left of them.

- **Symlinks are not representable, and `--follow-symlinks` is a lossy conversion, not support.**
  A VERSION 1 tree entry is `{name, oid, kind ∈ {Blob,Tree}, exec}` with no spare bit, so `import`
  refuses symlinks by default and names every one it found:

  ```text
  forge: invalid: import refuses /tree/escape.txt (1 more symlink(s) in this tree: /tree/link.txt); pass --follow-symlinks to materialise link targets that stay inside the import root (a VERSION 1 tree cannot represent a symlink; see docs/POSIX.md)
  ```

  With `--follow-symlinks` a link becomes a *copy* of its target: importing `link.txt -> real.txt`
  yields two entries with the **same blob id**
  (`c7325398…  link.txt` and `c7325398…  real.txt`), and exporting gives back two regular files, not
  a link. Containment still holds: a target resolving outside the import root is refused even with
  the flag — `import refuses /tree/escape.txt: target /etc/passwd is outside the import root /tree`.
  Real symlinks need a VERSION 2 tree entry that does not exist. See [`docs/POSIX.md`](docs/POSIX.md).
- **POSIX metadata is dropped or widened, silently.** `exec` is the only mode bit the format has.
  A directory holding `0600 a`, `0444 b` and `0755 x`, imported and exported, comes back:

  ```text
  -rw-r--r-- 0/0               2 1970-01-01 00:00 a
  -rw-r--r-- 0/0               2 1970-01-01 00:00 b
  -rwxr-xr-x 0/0               2 1970-01-01 00:00 x
  ```

  `0600` and `0444` both become `0644`; only the exec bit survives; uid/gid are zeroed and mtime is
  the epoch. Setuid, setgid, sticky, xattrs and ACLs are dropped, hardlinked pairs become two
  independent files, and a sparse file is materialised in full — those four are documented in
  [`docs/POSIX.md`](docs/POSIX.md) and were not re-measured for this release. A `chmod` after
  extraction recovers a mode; nothing recovers a symlink.
- **A blob must fit in memory, and reading costs 3x.** `put`/`get` take and return whole buffers.
  Measured on the box above with a 256 MiB blob: `write` peaked at **1.03x** the payload (262144 KiB
  of payload, 268904 KiB peak RSS — the publisher is copy-free), reading it back peaked at
  **3.03x** (793292 KiB) — durable read buffer, object-cache clone, and the decoded copy handed to
  the caller, all live at once. That cache is bounded by entry count (256), never by bytes.
  `forge write` warns above 64 MiB (`forge: warning blob 100000000 bytes > 64MiB`) and nothing
  refuses. Treat **RAM/3** as the practical ceiling for a single blob you intend to read.
- **One object is one file, so inodes bound the repository before bytes do.** Trying to build a
  repository past a million objects on this box, `import` of a 1,000,000-file tree stopped at
  **685,044 object files** with:

  ```text
  forge: io: No space left on device (os error 28)
  ```

  exit **5** — with **45% of the bytes still free** and `df -i` at **100%** (1,966,080 inodes, the
  default ratio for a 30 GB ext4). Size your filesystem by inodes, not by gigabytes: an object file
  averaged well under a kilobyte here. The repository was intact afterwards; `fsck --full` walked
  all 685,044 objects and returned `ok`.
- **The object-graph walk is bounded, and it is not resumable.** `fsck --full`, `gc` and seal
  verification hold the entire reachable set in memory — one `Vec`, one `HashSet`, one `HashMap` —
  so there is a ceiling, `DEFAULT_MAX_GRAPH_OBJECTS = 1_000_000`. Reaching it is **exit 1**, and the
  refusal says why and what to do; measured with the ceiling forced down to three:

  ```text
  forge: invalid: object graph walk reached this build's ceiling of 3 objects. The repository is not corrupt; this is a memory bound on the WALK, not a bound on any object, and no object was found damaged. Re-run with FORGEFS_MAX_GRAPH_OBJECTS=<n> above 3 (the walk holds roughly 100 bytes per object, so budget that much RAM), or reduce what is reachable -- `forge gc --dry-run` reports it -- and re-run. See docs/GC.md.
  ```

  In v0.3.0 that was `corrupt: object graph exceeded 1000000 objects`, **exit 2**, on intact bytes
  (#359). The classification is fixed; the bound is not gone. A repository past a million objects
  needs `FORGEFS_MAX_GRAPH_OBJECTS` and the RAM to match, and a resumable walk — the real answer —
  is not what v0.4.0 ships. The default ceiling was not reached on this box: the inode exhaustion
  above stopped it at 685,044 objects, where `fsck --full` returned `ok`.
- **A staged directory cannot be listed until it is published.** `write /d/x.txt` into a
  read-write mount, and `ls /` shows the new directory with a **zero tree id** and `read /d/x.txt`
  returns its contents — while `ls /d` answers `forge: not found: d`, exit 1. The destination of an
  `mv` behaves the same way before its checkin. Nothing is lost and the checkin publishes correctly;
  the listing of the parent and the listing of the child simply disagree about whether the directory
  is there. Measured on v0.4.0.
- **A no-op checkin and `abandon` disagree about what "staged" means (#342).** `Noop` clears only
  the published mount's overlay rows, while `abandon_session` counts rows across the whole
  namespace, so `checkin` can say "nothing to do" and `abandon` can still refuse the same session.
  Nothing is stranded — checking in on the other mount noops it and clears its rows, so the session
  always finishes — but the two verbs do disagree, and `model_composition.rs` reproduces it above
  roughly `FORGEFS_MODEL_SEQUENCES=20 FORGEFS_MODEL_STEPS=200`. The committed default run does not
  reach it. Recorded in [INVARIANTS.md](INVARIANTS.md) under "Shape gaps that remain".
- **`serve` is a strict subset of the CLI, and no invariant covers it (#332, residual).** The
  daemon serves 8 ops against the CLI's 25 verbs. As of v0.4.0 that surface is *specified* —
  `DAEMON_OPS` declares every op and field, unknown ones are refused, flags must be JSON booleans,
  error codes come from the same table as the CLI's exit codes, and `CLI_ABI.md` documents it — so
  it is no longer an undocumented wire format. What remains is the gap itself: 17 CLI verbs have no
  daemon op, and closing it is a product decision, not a contract repair. There is still no
  invariant that names the daemon.
- **The mount-time refusal closes #328 for new mounts only.** A read-write mount of a protected ref
  is now refused when you ask for it, so the wedge cannot be created. A catalog row written by an
  older build — an existing read-write mount on a protected ref with work already staged — still has
  no exit but `abandon session --discard-staged`. No escape was invented, because I5 mandates the
  CAS denial and there is genuinely nowhere for that work to land; a test pins that the work stays
  readable (I18).
- **Single node.** `forge serve` binds a Unix socket at `.forge/forge.sock` mode `0600`;
  `serve --http` adds a loopback listener on `127.0.0.1:4077` (`FORGE_HTTP_ADDR` moves it). There is
  no replication, no remote transport, no multi-host consensus. ForgeFS is a substrate for many
  agents on one machine.
- **Unix only.** No Windows binary is published and the workspace does not build there.
- **The undeclared-command backstop in the gate scripts needs bash ≥ 4.** On macOS's bash 3.2.57 it
  is absent — measured, and detailed under [the gates](#the-gates-need-nothing-but-bash-coreutils-sed-and-awk).
  The declared-list check works everywhere.
- **No published W6.** Large tree walk/update (10k/100k/1M entries) has no results. Do not infer
  tree scaling from the 32-agent numbers above.
- **Crash evidence is process-level, not power-loss.** A deterministic fault-injection matrix covers
  every durability transition, and real `SIGKILL` tests exist (`cli_sigkill.rs`). Neither is a
  power-loss claim; that half rests on `synchronous=FULL` staying FULL plus the device honouring
  flushes. [`docs/RECOVERY.md`](docs/RECOVERY.md) states the contract.
- **Termination by signal carries no exit code.** On a filesystem that runs out of space while
  SQLite's mapped wal-index has holes, a page fault arrives as SIGBUS. ForgeFS refuses to open below
  a 32 KiB free-space threshold rather than installing a handler, but automation must still
  distinguish `WIFSIGNALED` from every row of the exit-code table. `CLI_ABI.md` explains why.

Four entries that were on this list for v0.3.0 and are **gone because they were fixed**, not
because they were quietly dropped: `seal` not being CAS'd against the ref it names (#331); a
read-write mount of a protected ref wedging the session (#328, for new mounts); the observation
epoch, which is now the mount rather than the session, with I9 stating so (#329); and a large
repository being called corrupt rather than too large to walk (#359). All four are documented above
under [What changed in v0.4.0](#what-changed-in-v040-verb-by-verb) or in this list.

One entry that was **checked and found no longer true**: issue #349 reported that the `release`
GitHub Environment had no protection rules while `docs/RELEASING.md` said it did. As of this
release the environment has a required reviewer and a deployment branch policy restricted to `v*`
tags, so the documented control exists. The issue's second half still stands, and cutting this
release confirmed it: all three of the prepare-release PR's workflow runs (`ci`, `release`,
`security`) landed in `action_required` and sat there until each was approved with
`POST /repos/zozo123/forgefs/actions/runs/{id}/approve`. `docs/RELEASING.md` does not mention that
step.

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object ids and structured errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical typed objects and deterministic tree copy-on-write |
| `forge-store` | Crash-durable write-once CAS + atomic SQLite metadata, group-committed, with a read-only connection pool for catalog reads |
| `forge-cap` | `(operation, resource)` macaroon-style capabilities |
| `forge-ns` | Session mounts and overlay resolution |
| `forge-merge` | DAG merge bases, deterministic 3-way merge, Conflict objects |
| `forge-protocol` | The framed request/response envelope the daemon speaks |
| `forge-api` | Public facade: `repository`, `authority`, `workspace`, `refs`, `integration`, `import`, `export`, `gc`, `fsck`, `stats`, `serve` |
| `forge-cli` | `forge`; requires explicit `--cap` / `FORGE_CAP` |

[INVARIANTS.md](INVARIANTS.md) is the file to read second. [FORMAT.md](FORMAT.md) freezes the v1
object encoding. [AGENTS.md](AGENTS.md) has the contributor architecture and change rules.
[`docs/`](docs/) holds the GC, POSIX, benchmark, recovery, object-store, chunking and release
documents. Apache-2.0.
