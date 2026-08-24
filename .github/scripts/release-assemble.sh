#!/usr/bin/env bash
set -euo pipefail

: "${VERSION:?VERSION is required}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd payload

expected="$(mktemp)"
actual="$(mktemp)"
trap 'rm -f "$expected" "$actual"' EXIT
"$script_dir/release-assets.sh" "$VERSION" | LC_ALL=C sort >"$expected"
unexpected="$(find . -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
if [ -n "$unexpected" ]; then
  echo "::error title=Non-file release payload entry::$unexpected"
  exit 1
fi
find . -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort >"$actual"
if ! diff -u "$expected" "$actual"; then
  echo "::error title=Release payload mismatch::downloaded assets do not match the audited catalog"
  exit 1
fi
while IFS= read -r file; do
  test -s "$file"
  test ! -L "$file"
done <"$expected"

# SHA256SUMS is the manifest. Every other regular payload file is covered.
while IFS= read -r file; do
  sha256sum "$file"
done <"$expected" >SHA256SUMS
sha256sum --strict -c SHA256SUMS
test -s SHA256SUMS
