#!/usr/bin/env bash
set -euo pipefail

: "${VERSION:?VERSION is required}"
payload="${1:?usage: release-verify-payload.sh PAYLOAD_DIR}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$payload"

expected="$(mktemp)"
actual="$(mktemp)"
manifest="$(mktemp)"
expected_with_manifest="$(mktemp)"
trap 'rm -f "$expected" "$actual" "$manifest" "$expected_with_manifest"' EXIT
"$script_dir/release-assets.sh" "$VERSION" | LC_ALL=C sort >"$expected"
{
  cat "$expected"
  printf 'SHA256SUMS\n'
} | LC_ALL=C sort >"$expected_with_manifest"
unexpected="$(find . -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
if [ -n "$unexpected" ]; then
  echo "::error title=Non-file release payload entry::$unexpected"
  exit 1
fi
find . -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort >"$actual"
if ! diff -u "$expected_with_manifest" "$actual"; then
  echo "::error title=Release payload mismatch::payload does not match the audited catalog"
  exit 1
fi
while IFS= read -r file; do
  test -s "$file"
  test ! -L "$file"
done <"$actual"
sha256sum --strict -c SHA256SUMS
cut -c67- SHA256SUMS | LC_ALL=C sort >"$manifest"
if ! diff -u "$expected" "$manifest"; then
  echo "::error title=Checksum manifest mismatch::SHA256SUMS does not cover the exact payload"
  exit 1
fi
