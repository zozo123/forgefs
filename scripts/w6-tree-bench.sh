#!/usr/bin/env bash
# W6 large-tree walk/update harness wrapper (#24).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

entries="${1:-10000}"
workers="${2:-1}"

forge_bin="${FORGE_BIN:-target/release/forge}"
if [[ ! -x "$forge_bin" ]]; then
  cargo build --release --locked -p forge-cli
  forge_bin="target/release/forge"
fi

exec "$forge_bin" bench --tree-entries "$entries" --workers "$workers" --agents 0 --shared 0
