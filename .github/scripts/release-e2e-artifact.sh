#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"

name="forge-$VERSION-$TARGET"
unpack="$(mktemp -d)"
expected="$(mktemp)"
members="$(mktemp)"
trap 'rm -rf "$unpack"; rm -f "$expected" "$members"' EXIT
printf '%s\n' \
  "$name/" \
  "$name/CLI_ABI.md" \
  "$name/INVARIANTS.md" \
  "$name/LICENSE" \
  "$name/README.md" \
  "$name/forge" | LC_ALL=C sort >"$expected"
tar -tzf "dist/$name.tar.gz" | LC_ALL=C sort >"$members"
diff -u "$expected" "$members"
tar -xzf "dist/$name.tar.gz" -C "$unpack"
test -d "$unpack/$name"
test ! -L "$unpack/$name"
shopt -s dotglob nullglob
entries=("$unpack/$name"/*)
test "${#entries[@]}" -eq 5
for entry in "${entries[@]}"; do
  test -f "$entry"
  test ! -L "$entry"
done
for file in CLI_ABI.md INVARIANTS.md LICENSE README.md forge; do
  test -s "$unpack/$name/$file"
  test ! -L "$unpack/$name/$file"
done
bin="$unpack/$name/forge"
chmod +x "$bin"
test "$("$bin" --version | awk '{print $2}')" = "$VERSION"
scripts/release-gate.sh "$bin" evidence
