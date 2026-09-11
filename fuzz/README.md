# Fuzzing evidence and triage

The `fuzz` workflow compiles all six targets on relevant PRs and runs each for
60 seconds. Nightly and manual runs extend that budget to 600 seconds per
target. All runs install the nightly toolchain explicitly and use independent
jobs with a 2 GiB libFuzzer RSS limit. One failing
target does not cancel the others. Compilation and startup count toward the
job's 30-minute limit, not the 600-second fuzzing budget.

Each job uploads its generated corpus, crash inputs, full execution log and
JSON outcome as `fuzz-<target>-<run-id>-<attempt>` for 30 days, including on
failure. The outcome identifies the tested commit and command. A crash,
timeout, or inability to launch cargo fails the job; evidence upload does not
turn it green. Setup failures can have no harness evidence and remain failures.

To reproduce locally from the repository root:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --version 0.13.2 --locked
FORGE_FUZZ_TARGET=object_decode python3 .github/scripts/run-fuzz.py
```

For a failing run:

1. Download the target's artifact before retention expires and record the
   workflow URL, commit, target, JSON outcome and diagnostic in a bug issue.
2. Check out that commit, install the same toolchain shown in the Actions setup
   log, and reproduce the input with
   `cargo +nightly fuzz run <target> <crash-file>`.
3. Minimize it with `cargo +nightly fuzz tmin <target> <crash-file>`. Add a named
   invariant regression alongside the affected crate's tests before fixing it.
4. Rerun the regression and fuzz target, then link the passing evidence in the
   fixing PR. Do not suppress the crash or relax the decoder to clear a run.

Corpus artifacts preserve each run for reuse and investigation; they are not
automatically restored into later runs. A longer configured budget is not proof
of a completed run. These checks also do not implement Loom or replace the
deterministic interleaving/model-checking work still tracked by #23.
