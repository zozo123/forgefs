#!/usr/bin/env bash
set -euo pipefail

cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --locked -p forge-cli
scripts/cli-abi-conformance.sh target/debug/forge
scripts/release-gate.sh target/debug/forge
git diff --check
