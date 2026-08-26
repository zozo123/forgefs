#!/usr/bin/env bash
# Produce the attested CycloneDX SBOM for one release target.
#
# Tool choice is recorded in docs/SUPPLY-CHAIN.md. The SBOM is generated from
# the committed lockfile for the exact target triple, normalised into a
# deterministic document bound to the release commit, and then cross-checked
# against the crate paths rustc embedded in the binary that actually ships.
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"
: "${COMMIT_SHA:?COMMIT_SHA is required}"
test "$(git rev-parse HEAD)" = "$COMMIT_SHA"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(pwd)"
bin="target/$TARGET/release/forge"
test -x "$bin"

name="forge-$VERSION-$TARGET"
mkdir -p out

raw_dir="$(mktemp -d)"
trap 'rm -rf "$raw_dir"' EXIT
rm -f crates/forge-cli/*.cdx.json

cargo cyclonedx \
  --manifest-path crates/forge-cli/Cargo.toml \
  --target "$TARGET" \
  --format json \
  --spec-version 1.5 \
  --all \
  --describe binaries \
  --quiet

produced="$(find crates/forge-cli -maxdepth 1 -name '*.cdx.json' -type f | LC_ALL=C sort)"
test "$(printf '%s\n' "$produced" | wc -l)" -eq 1
mv "$produced" "$raw_dir/raw.cdx.json"

python3 "$script_dir/release-sbom.py" normalize \
  --input "$raw_dir/raw.cdx.json" \
  --output "out/$name.cdx.json" \
  --workspace-root "$root" \
  --version "$VERSION" \
  --commit "$COMMIT_SHA" \
  --target "$TARGET" \
  --rustc "$(rustc --version)" \
  --source-date-epoch "$(git show -s --format=%ct "$COMMIT_SHA")"

python3 "$script_dir/release-sbom.py" verify-binary \
  --sbom "out/$name.cdx.json" \
  --binary "$bin"

test -s "out/$name.cdx.json"
