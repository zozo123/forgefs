#!/usr/bin/env bash
# Print the complete, immutable release payload catalog (without SHA256SUMS).
set -euo pipefail

version="${1:?usage: release-assets.sh VERSION}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$ ]]; then
  echo "invalid release version: $version" >&2
  exit 2
fi

targets=(
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin
)
gated_targets=(
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin
)
evidence=(
  abi-conformance.json
  conflict-object.txt
  env-line.json
  env-line.txt
  fsck-full.json
  gate-summary.json
  seal-attestation.txt
)

for target in "${targets[@]}"; do
  printf 'forge-%s-%s.BUILD-INFO.txt\n' "$version" "$target"
  printf 'forge-%s-%s.tar.gz\n' "$version" "$target"
done
for target in "${gated_targets[@]}"; do
  for file in "${evidence[@]}"; do
    stem="${file%.*}"
    extension="${file##*.}"
    printf '%s-%s.%s\n' "$stem" "$target" "$extension"
  done
done
