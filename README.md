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
measured below a `forge` CLI checkin is slower than a `git commit` in a worktree (see
[What it costs](#what-it-costs) — the comparison is not durability-equivalent in Git's favour).
ForgeFS buys isolation, recorded reasoning, and provenance. It costs durability barriers per
publication, and it is not a POSIX filesystem.

## Install

Released binaries exist for four targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. **There is no Windows build and the source does not
compile on Windows** — production code uses `std::os::unix` unconditionally.

```bash
V=0.2.1; T=x86_64-unknown-linux-gnu
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/forge-$V-$T.tar.gz
curl -sSLO https://github.com/zozo123/forgefs/releases/download/v$V/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf forge-$V-$T.tar.gz
sudo install forge-$V-$T/forge /usr/local/bin/
forge --version
```

```text
forge-0.2.1-x86_64-unknown-linux-gnu.tar.gz: OK
forge 0.2.1
```

On macOS the same commands work with `shasum -a 256 --ignore-missing -c SHA256SUMS`; run against
`T=aarch64-apple-darwin` it prints `forge-0.2.1-aarch64-apple-darwin.tar.gz: OK`, and the extracted
binary reports `forge 0.2.1`. The tarball also contains `README.md`, `INVARIANTS.md`, `CLI_ABI.md`
and `LICENSE`.

`SHA256SUMS` has 36 entries. It covers the four binaries *and* the release-gate evidence published
beside them — the ABI conformance table, the `fsck --full` report, the seal attestation, the
environment line and the gate summary, per target. Each release is also covered by a SLSA
provenance attestation you can check without trusting this page:

```bash
gh attestation verify forge-$V-$T.tar.gz -R zozo123/forgefs
```

```text
Loaded digest sha256:a8a0323214d95e7d413583cdae07ebe681a57fcbaa3b52204835e3f052863a8b for file://forge-0.2.1-aarch64-apple-darwin.tar.gz
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
  - Build workflow:. .github/workflows/release.yml@refs/tags/v0.2.1
  - Signer repo:.... zozo123/forgefs
  - Signer workflow: .github/workflows/release.yml@refs/tags/v0.2.1
```

`gh attestation verify` prints that only to a terminal; redirected to a pipe or a file it prints
nothing and communicates through its exit status alone. Check `$?`, not the output.

### Which build this page documents

`v0.2.1` is the latest release. Several things documented below **postdate it** and are only
available from `main` until the next release. Checked against the shipped v0.2.1 binary:

| Documented here | In v0.2.1? |
|---|---|
| `gc --collect` (reclamation actually unlinks) | no — v0.2.1 prints `--dry-run … Required. Collection is not implemented` |
| `checkin --mount <path>` | no |
| `import --follow-symlinks` | no |
| per-read-write-mount pinning (I19–I21), the I22 checkin refusal | no |
| everything else on this page | yes |

The worked example below runs unchanged on the released v0.2.1 binary, and the outputs shown for it
were produced by exactly that binary. The reclamation section and a few of the limits need a build
from `main`:

```bash
cargo install --locked --git https://github.com/zozo123/forgefs forge-cli
```

```text
  Installing /usr/local/cargo/bin/forge
   Installed package `forge-cli v0.2.1 (https://github.com/zozo123/forgefs#2b463448)` (executable `forge`)
```

Note that `cargo install --git` tracks whatever `main` is when you run it; it printed `#2b463448`
here.

or, in a clone, `cargo build --locked --release` and put `target/release/forge` on `$PATH`. It needs
the pinned toolchain in `rust-toolchain.toml` (Rust 1.97.0); the MSRV floor is 1.89.

Every command on this page was executed. The worked example ran against the installed **v0.2.1
release binary**; the reclamation section, the gates and the benchmark ran against a build of commit
`2b4634488f49450537019fff0b5b4d1436f5181a` (which also reports `forge 0.2.1`). Object ids for
*content* are reproducible and you should see the same ones — `b6b49a01…` for `pub fn a() {}` in
both the worked example and the reclamation example below. Commit and tree ids embed a timestamp, so
those will differ on your machine.

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
imported 5a8b42b6c7930a8abf0136fff48d6a01051c8d0e8463e4e7ffbe4d391a98d915 -> work
P- commit                           main 2ad4e449d197d80ec81dfb9e7d2291bc2648d51b95f31ed27853373a1990c2e0
-- commit                           work 5a8b42b6c7930a8abf0136fff48d6a01051c8d0e8463e4e7ffbe4d391a98d915
```

The two flag columns are `P` protected and `S` sealed. `main` is protected at `init`: only `merge`
and `seal` may advance it, so agents never publish to it directly. `import` refuses symlinks by
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

Both exit 1. A capability is a 310-byte token carrying `(operation, resource)` caveats. Attenuation
can only shrink it, and a namespace id is not authority (I13, I14).

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
updated work e06b1e2b6d446a22b9d46d6496fb0770bda9d556b49ef442e2271dbf09ff5c7e
forked work -> forks/work/bob/01M0WYBPQ7HX9YVYEEY2BW326E ours=d3b6644a143d9007d45b6dd47857208eecd4f02ea5e892e2d251267bb89e9ed4 theirs=e06b1e2b6d446a22b9d46d6496fb0770bda9d556b49ef442e2271dbf09ff5c7e
-- commit                           forks/work/bob/01M0WYBPQ7HX9YVYEEY2BW326E d3b6644a143d9007d45b6dd47857208eecd4f02ea5e892e2d251267bb89e9ed4
-- commit                           heads/agents/alice/01M0WYBPMESYENG6CR719RJHFT 5a8b42b6c7930a8abf0136fff48d6a01051c8d0e8463e4e7ffbe4d391a98d915
-- commit                           heads/agents/bob/01M0WYBPMM0MTE8WZ82SC575AN 5a8b42b6c7930a8abf0136fff48d6a01051c8d0e8463e4e7ffbe4d391a98d915
P- commit                           main 2ad4e449d197d80ec81dfb9e7d2291bc2648d51b95f31ed27853373a1990c2e0
-- commit                           work e06b1e2b6d446a22b9d46d6496fb0770bda9d556b49ef442e2271dbf09ff5c7e
```

**Both checkins exit 0.** Losing a race is a normal outcome, not an error, and the fork is a real
ref holding Bob's completed contribution (I18):

```bash
FORK=$(forge --cap $ROOT refs | awk '$3 ~ /^forks\// {print $3}' | head -1)
C=$(forge --cap "$BOB" session open --from="$FORK")
forge --cap "$BOB" ls --ns "$C" /
```

```text
blob  - f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc bob.rs
blob  - 6c32ed274accb8fc6a03f46ee042e3fb883b847395ea9b178263c217825584fc main.rs
```

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
imported 5ec1cee7363de85b86370f6aec0c44fc073ceb409bc4a8bd74887d95e5371d3c -> deps
mounted / -> ref:work
mounted /vendor -> ref:deps
API = 1
mounted / -> ref:deps
9af68071c07115c84a52289c06bd93f4985ab058dbf490df168e054e445c088a
updated deps 52909d6e85470eee938f28b0bdfd401ff98373e5c49aa9e2d10e758df61fe513
07832846edcdd594e7461e79c6c15e4b2944f8b059ab68b6b057ff3f53d58220
forge: stale observation of /vendor:/api.txt: expected 0aee50505cca4e134e62f9098aa20457db4b5b857e8acb2bf83b7f652c8f62c3, found 9af68071c07115c84a52289c06bd93f4985ab058dbf490df168e054e445c088a
```

**Exit 4.** Carol's write does not collide with anything. She is refused because the file she
*reasoned from* is no longer the file she read. That is the property that makes an agent's output
trustworthy, and it is the one thing a lock-free shared checkout cannot give you. Her staged work is
not destroyed; the refusal names the mount and the path.

### Integrate, seal, verify

```bash
forge --cap $ROOT merge --into=main --from=work
forge --cap $ROOT merge --into=main --from="$FORK"
forge --cap $ROOT seal main --tag v1.0 --attest
forge --cap $ROOT verify v1.0
forge --cap $ROOT fsck --full
```

```text
merged main f9b07352275bbe77f210030af11bcf12363f0aa1ec10d3d4cec1c81c3d2b65be
merged main b917900b301c0bb84019c5b1aadd4c5359d5d2dafa2f23bac28723ff02047717
sealed tags/v1.0 e3d4b50326c7633e1f302447b85cb8af32737bbe97f6005df3b91d8804a9e2e9
attested ok
ok e3d4b50326c7633e1f302447b85cb8af32737bbe97f6005df3b91d8804a9e2e9
ok (full): 10 refs, 26 objects, 5 namespaces
```

Bob's fork merged cleanly because the two agents touched different paths. Had they touched the same
path, `merge` would have exited 4 and produced a **Conflict object** under `conflicts/` holding both
immutable inputs, rather than a string in a terminal.

## The model

Eight nouns are enough to reason about everything above.

| | |
|---|---|
| **object** | Immutable bytes. `ObjectId = BLAKE3(canonical encoding)`. Blob, Tree, Commit, Contribution, Conflict, Snapshot. Written once, never overwritten. |
| **ref** | The only mutable thing in the system. Every move is `CAS(expected → new)`. `heads/`, `forks/`, `conflicts/`, `tags/` are *types*, not naming conventions. |
| **session** | `(capability, namespace, pin, observations)`. Opening one pins a commit. |
| **mount** | A path in the session bound to a spec. `ref:NAME --rw` makes that ref the publication target. Every read-write mount carries **its own** pinned base, so reads never come from a live ref another agent can move. |
| **observation** | Every read records `path → what it saw`: a blob id, a tree id, or *absence*. Absence counts; silence does not. |
| **contribution** | A checkin is a typed Contribution object bound to the agent, not a log message. |
| **capability** | `(operation, resource)`. Attenuation only shrinks. There is no ambient root. |
| **seal** | An ed25519 signature over a snapshot. `verify` re-reads durable bytes and walks the typed graph against *this forge's* trusted key. |

One publication, exactly:

```text
write            -> stage into the mount's overlay
checkin --mount  -> fold that overlay onto that mount's pin
                 -> build a Contribution
                 -> re-check every observation
                 -> CAS the ref that mount names, from that pin

  CAS won                       exit 0   updated <ref> <oid>
  CAS lost                      exit 0   forked <ref> -> forks/... (work preserved)
  an observation moved          exit 4   stale observation of <mount>:<path>
  same path both sides at merge exit 4   Conflict object under conflicts/
  nothing to publish, but the
  session holds staged work     exit 1   names the mounts that still hold it
```

Automation keys on those exit codes, never on stderr wording: `4` is a stale observation or a merge
conflict, `2` is corruption or a sealed-state violation, `1` is denial or bad input, `3` is
transient contention, `5` is I/O or internal failure. [`CLI_ABI.md`](CLI_ABI.md) is the contract,
and 45 of its 46 rows are executable and blocking.

## Why you should believe any of this

[INVARIANTS.md](INVARIANTS.md) is 23 numbered rules, I1 through I23. That file is not a manifesto,
and this is the part that is genuinely unusual:

**Every rule names its production owner and its test.** The file ends with a traceability table
mapping each invariant to the module that implements it and to the exact test files that prove it.
I18 ("a refused checkin never destroys staged work") points at `forge-api/workspace.rs`,
`forge-api/gc.rs`, `forge-store/meta.rs`, and at `pinned_rw_session_reads.rs`,
`cli_shared_stampede.rs`, `gc_and_abandon.rs`. You can check any claim on this page by opening the
row.

**A PR that cannot name an invariant does not merge.** That is a stated rule in INVARIANTS.md,
enforced by review rather than by CI. When a fix needs a rule that does not exist yet, the rule gets
added: I19–I21 arrived with the multi-mount pinning fix, I22 with the checkin refusal, I23 with
garbage collection.

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
could see it. The generator is the same seeded xorshift idiom as the property tests, with no
property-testing dependency, and a failure prints the seed and the whole operation trace. Its
`KNOWN` table of characterised defects is asserted to be *fully observed*, so fixing one of them
fails the test until the row is deleted — an allow-list that cannot rot. Four tests, 5.06 s.

**The release verifies itself, and you can re-run that.** `scripts/release-gate.sh` drives a fresh
repository through the whole contract with a packaged binary — same-path overlap producing a
Conflict object, a stale-observation refusal, seal + attest + verify, `fsck --full`, and the full
CLI ABI table — and it runs on all four targets during the release, from the packaged binary rather
than a rebuild. Its evidence is published beside the tarballs and covered by `SHA256SUMS`.

```bash
bash scripts/release-gate.sh target/release/forge
```

```text
release-gate: PASS - forge 0.2.1 sealed and verified itself as v0.2.1
```

Its `release-gate-out/gate-summary.json` records what each phase actually proved. Four of the
`phases_passed` entries, `id` and `detail`:

```text
gate/same-path-overlap
  merge exit 4, Conflict 13887a383a635f45b576311ef71bd33430def0482f44a117018b71eaa6ede315 at conflicts/main/01M0WZH53PS5HR0HYCBDYF8G4F, main pinned at f65beb75bcdd29ff32dc7f14b3280ff6bb13da319e63464731e00d0e502128de
gate/stale-observation
  checkin exit 4, heads/agents/bob/01M0WZH57HJQK0MVWJRW8CVNBZ pinned at '54512e6ce9b41ebfdc2a9229f9f59862616bd1ea46a7cf3ffdbc396fb6895d43', control ref advanced to 8da26b1d4e4a4ce84168743071920db5f557cbe9f48da36a185cebf4ccaab14b
gate/seal-verify
  sealed tags/v0.2.1 -> df5228f31a883f7dae6ce16e570fc54323d20206718fcb46c1bdd381e33cc86d, --attest ok, verify ok, flags PS
gate/fsck
  fsck --full --json ok: refs=13 objects=40 namespaces=10 findings=0
```

The gate is versioned with the contract, not with the binary, so it is also a skew detector. Run the
current `main` gate against the shipped v0.2.1 tarball and it correctly fails one row —
`FAIL abi/0-gc-collect expected=0 observed=1` — because `gc --collect` postdates that release. Use
the gate evidence published with a release to check that release, and the in-tree gate to check the
tree.

At `2b46344`, on the box described below, the local gates produce: `cargo fmt --all -- --check`
clean; `cargo clippy --workspace --all-targets --locked -- -D warnings` clean; **360 tests passed,
0 failed** across 99 test binaries; `abi rows=46 blocking=45 known_failing=0 unexercised=1
blocking_failures=0`; and the release-gate PASS above.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
bash scripts/cli-abi-conformance.sh target/release/forge
bash scripts/release-gate.sh target/release/forge
```

## Reclamation

> Needs a build from `main`. The released v0.2.1 binary answers `gc --collect` with
> `error: unexpected argument '--collect' found`.

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

FORK=$(forge --cap $ROOT refs | awk '$3 ~ /^forks\// {print $3}' | head -1)
forge --cap $ROOT abandon session "$A" --discard-staged
forge --cap $ROOT abandon session "$B" --discard-staged
forge --cap $ROOT abandon fork "$FORK"

find $FORGE_DIR/.forge/objects -type f | wc -l
sleep 65                       # --collect refuses below a 60-second age floor
forge --cap $ROOT gc --collect --min-age-secs 60
find $FORGE_DIR/.forge/objects -type f | wc -l
forge --cap $ROOT fsck --full
```

```text
initialized ./gcdemo/.forge
root cap: ./gcdemo/.forge/keys/root.cap
imported 9d4ac8150eb626551632fa02cd0d49d7e6ed975398ae3bac76abc2af900385c6 -> work
mounted / -> ref:work
mounted / -> ref:work
b6b49a013ebdcf7bc69efd9d31b64bc2e458e40f77d898c63a4417f4da4bf1a3
f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc
updated work 5b0095366737d376476512d50abacbf325e7ca0cfe78b7e691231a4eb9b50f69
forked work -> forks/work/anon/01M0WYGYRRJSKCF556WM54BDTW ours=d0c2cbac1465a1837fb7f46f66df01335751b622d28414f49966b5bc344bebed theirs=5b0095366737d376476512d50abacbf325e7ca0cfe78b7e691231a4eb9b50f69
abandoned session 01M0WYGYQ2XMHZ0HTDYA5VKXNH discarded=0 mounts=2 observations=0
abandoned session 01M0WYGYQEHB73M28S4TWC35Z3 discarded=0 mounts=2 observations=0
abandoned forks/work/anon/01M0WYGYRRJSKCF556WM54BDTW commit d0c2cbac1465a1837fb7f46f66df01335751b622d28414f49966b5bc344bebed
13
gc (collect, min-age 60s): 9 of 13 objects reachable
roots: 4 refs (0 unresolved forks), 0 session pins, 0 live refs, 0 mounts, 0 mount pins, 0 overlay blobs, 0 observations, 1 landmarks, 0 seals
collectable: 4 objects, 473 bytes
withheld (younger than min-age): 0 objects, 0 bytes
collectable 7976b43e084d158f90feb52c29449a1ac4ddba0f82afd01e92536d53b9d34254
collectable 82ce8d3d2b8d3d437f78a2d23d3e38f1110acbf8ba951edd57fb94a75b1ab566
collectable d0c2cbac1465a1837fb7f46f66df01335751b622d28414f49966b5bc344bebed
collectable f0d143179bf0da97bc06a0a31f8397ad207eb47d4ad1ff37e21ddcc48bffccdc
collected: 4 objects unlinked
9
ok (full): 4 refs, 9 objects, 0 namespaces
```

`--min-age-secs` is a hard floor, not a hint: `--collect` below 60 seconds is refused, because an
object is fsynced before the catalog row that roots it (I4), so a lower floor can collect live data.
`abandon fork` refuses while any session still mounts the ref, and `abandon session` refuses a
session holding staged work unless you pass `--discard-staged`. `--dry-run` computes the same plan
and deletes nothing. [`docs/GC.md`](docs/GC.md) has the root set and the sweep-race argument.

## What it costs

Numbers below are from `forge bench`, five fresh repositories per configuration, medians, on one
box. They are a regression signal for *this* hardware, not a "fastest filesystem" claim.

Environment, emitted verbatim by `scripts/forge-env-line.sh`:

```text
forgefs commit:        2b4634488f49450537019fff0b5b4d1436f5181a
build profile:         release
forge --version:       forge 0.2.1
rustc:                 rustc 1.97.0 (2d8144b78 2026-07-07)
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
repository class:      /workspace/envrepo (fresh repository per invocation)
```

**Barrier reach was established first, and it matters more than any number here.** The sandbox
mounted `/workspace` `nobarrier`, under which no durability measurement means anything. After
`mount -o remount,barrier /workspace`, 200 `fsync` calls moved `/proc/diskstats` field 19 by exactly
200 — ratio 1.00. Every figure below was taken after that, on `/dev/vdd`, not on the overlay `/tmp`.

`forge bench --agents 32 --shared 16 --workers W`, median of 5:

| W | private checkin ops/s | p50 | p99 | shared stampede p50 | device flushes / run | SQLite commits / run |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 242.8 | 16.06 ms | 18.47 ms | 10.10 ms | 1290 | 227 |
| 16 | 356.6 | 39.01 ms | 59.09 ms | 27.82 ms | 1018 | 174 |
| 64 | 354.7 | 83.93 ms | 89.08 ms | 31.40 ms | 976 | 163 |

Serial checkin — one agent at a time, true operation latency — is p50 **7.31–8.24 ms** across all
three. The shared stampede returned `updated=1 forked=15` in all fifteen runs, and every run ended
`busy=0 denied=0 stale=0 conflict=0`, with lifetime `fsync_file=321` and `fsync_dir≈830` regardless
of worker count.

**Do not over-read the absolute numbers.** This is a shared virtual machine. Three independent
campaigns of this same protocol during one afternoon put W=16 throughput at 525.7, 494.1 and 356.6
ops/s — a 1.5x spread across campaigns, wider than the effects being discussed. What is stable
across all three is the *shape*: the SQLite commit count falls with concurrency, the device-flush
count falls with it, p50 rises monotonically, and the serial floor sits near 8 ms.

Three honest readings.

**The floor is a durability barrier.** One uncontended checkin is `write → fsync(file) → exclusive
publish → fsync(parent directory)` and costs ~8 ms on this disk. Lifetime counters for the run
are `fsync_file=321` and `fsync_dir≈830` — **directory** barriers outnumber file barriers 2.6 to 1,
and that is where the cost sits.

**Group-committing the SQLite catalog is a real win, and it is visible in the counters.** As
concurrency rises the number of durable SQLite commits per run falls 227 → 174 → 163 and total
device flushes fall 1290 → 976, because N waiting writers now share one WAL fsync
(`synchronous=FULL` untouched). The change that introduced it measured **+33% throughput at 16 concurrent
writers**, 2.82x fewer durable commits, and mutex wait down from 31.6 ms to 9.5 ms, on the machine
where it landed. It is absent at W=1 and W=2, exactly as the mechanism predicts;
[`docs/BENCH.md`](docs/BENCH.md) explains why membership in a shared fsync is structural rather than
statistical.

**Fewer `fsync` calls does not mean faster.** The counters above make the point on their own:
`fsync_dir` is ~830 per run at every worker count, yet actual device flushes fall from 1290 to 976
and throughput rises by half. The barriers were already overlapping. The obvious next optimisation —
collapsing the nine directory barriers of an object publication down to two with I4 intact — has
been measured under issue #177 and **lost 15–22% at W=2..16**: a barrier that follows other barriers
costs about 49 µs against a 402 µs average, and jbd2 already merges concurrent fsyncs, so collapsing
removes the cheap barriers and gives up the overlap. That measurement is still under review and has
not landed; it is recorded here because it contradicts the naive model, not because anything shipped
from it. Count device flushes, not `fsync` calls.

**Throughput and latency do not move together.** Going from 4 to 16 workers on 4 logical cores buys
47% more throughput and costs 2.4x the p50; going on to 64 buys nothing measurable and costs another
2.2x. If you care about tail latency, oversubscribing the CPU is the first thing to stop doing.

**ForgeFS versus Git, stated against itself.** The checked-in comparator (`scripts/w7-git-comparator.sh`,
results in [`docs/BENCH.md`](docs/BENCH.md)) measured, like for like — one process invocation per
agent step, which is how an orchestrator drives either tool — **ForgeFS 198.3 ops/s against git
worktrees at 289.2**. ForgeFS loses. It is also doing strictly more durability work: for one agent
operation ForgeFS issued 6 file and 20 directory barriers, Git as shipped issued 0 and 0, and Git
with `core.fsync=all` issued 6 and 0. Both Git configurations are therefore marked
**`non-comparable: durability mismatch`** and neither quotient is a speed ratio.

Two things `forge bench` and `forge stats --json` deliberately do **not** report: object-byte
accumulation is uninstrumented and prints the literal `bytes=unavailable`, and the per-checkin cost
mix (`hash + encode + fsync_file + fsync_dir + sqlite_wait + sqlite_txn`) is reported as
`unavailable` and must not be reconstructed by dividing lifetime totals by a checkin count. Those
counters are cumulative process-lifetime totals spanning init, both workloads, merge/seal, verify
and `fsck`. [`docs/BENCH.md`](docs/BENCH.md) owns the protocol.

## Limits

Each of these was re-checked against the binary built from `2b46344` before being written down.

- **Symlinks are not representable, and `--follow-symlinks` is a lossy conversion, not support.**
  A VERSION 1 tree entry is `{name, oid, kind ∈ {Blob,Tree}, exec}` with no spare bit, so `import`
  refuses symlinks by default and names every one it found. With `--follow-symlinks` a link becomes
  a *copy* of its target: importing `link.txt -> real.txt` yields two entries with the **same blob
  id**, and exporting gives back two regular files, not a link. Containment still holds: a target
  resolving outside the import root is refused with `import refuses symlink <path>: target
  /etc/passwd is outside the import root <root>`. Real symlinks need a VERSION 2 tree entry that
  does not exist. See [`docs/POSIX.md`](docs/POSIX.md).
- **POSIX metadata is dropped or widened, silently.** `exec` is the only mode bit the format has.
  `0600` and `0444` both come back `0644`; setuid, setgid, sticky, mtime, uid/gid, xattrs and ACLs
  are dropped; hardlinked pairs become two independent files; a sparse file is materialised in full
  (a 100 MiB sparse file measured 101 MiB of objects). Documented, not fixed — a `chmod` after
  extraction recovers a mode, nothing recovers a symlink.
- **A blob must fit in memory, and reading costs 3x.** `put`/`get` take and return whole buffers.
  Measured on this box with a 256 MiB blob: writing peaked at **1.03x** the payload (the publisher
  is copy-free), reading it back peaked at **3.03x** — durable read buffer, object-cache clone, and
  the decoded copy handed to the caller, all live at once. That cache is bounded by entry count
  (256), never by bytes. `forge write` warns above 64 MiB
  (`forge: warning blob 268435456 bytes > 64MiB`) and nothing refuses. Treat **RAM/3** as the
  practical ceiling for a single blob you intend to read.
- **The observation epoch is per-session for observations but per-mount for overlay (#329).** A
  successful checkin on *any* mount clears the whole session's observation set. Verified: a session
  reads `/vendor/api.txt` through a read-only mount, checks in on `/`, another agent then moves
  `deps`, and the session's next checkin on `/` succeeds — the earlier cross-mount read has been
  forgotten. Within one checkin, cross-mount staleness *is* detected (that is the exit 4 above).
  I9 does not state its epoch; the two cleanup statements disagree.
- **`seal` is not CAS'd against the ref it names (#331).** `seal` reads the ref, builds the
  snapshot, and commits the tag; `Meta::commit_seal` takes no expected OID and the CLI has no way to
  express one. Every other ref move in the system is CAS'd (I5). The sealed snapshot stays
  internally consistent and `verify` still passes, so this is a correctness-of-naming gap, not
  corruption: "v1.0 is this ref at this moment" can be false with no error if the ref moves during
  the seal.
- **The `serve` daemon is outside the documented ABI (#332).** `CLI_ABI.md` describes the CLI and
  `scripts/cli-abi-conformance.sh` exercises the CLI; neither says anything about the daemon, and no
  invariant covers it. Its replies are not the documented surface — `POST /v1/ns.checkin` returns a
  Rust `Debug` rendering of the internal outcome enum:

  ```json
  {"v":1,"id":1,"ok":true,"body":"Updated { name: \"work\", oid: ObjectId(528cb4301bdae8a5c06d786f975ebb42aa481b6a4c09c19a256713469d69b2dc) }"}
  ```

  Do not build against `serve` expecting the CLI contract to hold.
- **A read-write mount on a protected ref wedges the session.** `mount / ref:main --rw` is accepted,
  `write` through it is accepted, `checkin` then denies (`ref main is protected; session checkin
  cannot advance it`, exit 1), and `abandon session` refuses because work is staged. Neither publish
  nor plain abandon is possible; only `abandon session --discard-staged` gets out, and that throws
  the work away. I20 closed the other two shapes of this (a read-write `oid:` mount and a ref not
  holding a commit are refused at mount time); a protected ref is still accepted at mount time and
  is still unpublishable. This is the one entry left in the composition harness's `KNOWN` table.
- **Single node.** `forge serve` binds a Unix socket at `.forge/forge.sock` mode `0600`;
  `serve --http` adds a loopback listener on `127.0.0.1:4077`. There is no replication, no remote
  transport, no multi-host consensus. ForgeFS is a substrate for many agents on one machine.
- **Unix only.** No Windows binary is published and the workspace does not build there.
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

## Layout

| Crate | Role |
|---|---|
| `forge-types` | Object ids and structured errors (`StaleObservation`, `Denied`, …) |
| `forge-core` | Canonical typed objects and deterministic tree copy-on-write |
| `forge-store` | Crash-durable write-once CAS + atomic SQLite metadata, group-committed, with a read-only connection pool for catalog reads |
| `forge-cap` | `(operation, resource)` macaroon-style capabilities |
| `forge-ns` | Session mounts and overlay resolution |
| `forge-merge` | DAG merge bases, deterministic 3-way merge, Conflict objects |
| `forge-api` | Public facade: `repository`, `authority`, `workspace`, `refs`, `integration`, `import`, `export`, `gc`, `fsck`, `stats`, `serve` |
| `forge-cli` | `forge`; requires explicit `--cap` / `FORGE_CAP` |

[INVARIANTS.md](INVARIANTS.md) is the file to read second. [FORMAT.md](FORMAT.md) freezes the v1
object encoding. [AGENTS.md](AGENTS.md) has the contributor architecture and change rules.
[`docs/`](docs/) holds the GC, POSIX, benchmark, recovery, object-store, chunking and release
documents. Apache-2.0.
