#!/usr/bin/env bash
set -euo pipefail

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git fetch origin main
git merge --no-edit origin/main

# Product changes are already materialized on agents/inbox-111-v3. From here
# this trusted, default-branch-owned job only proves they still compose with the
# latest main before advancing the PR branch.
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked

git push origin HEAD:agents/inbox-111-v3
