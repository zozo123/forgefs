#!/usr/bin/env bash
# Build one release target and package it deterministically.
#
# Determinism is a property of the packaging step, not an aspiration: member
# order, member names, modes, owner and mtime are all fixed from the tagged
# commit rather than inherited from the builder, and the gzip stream carries
# neither a name nor a timestamp. .github/scripts/release-reproduce.sh proves
# this by rebuilding the same commit in a second clean tree.
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"
: "${COMMIT_SHA:?COMMIT_SHA is required}"
test "$(git rev-parse HEAD)" = "$COMMIT_SHA"

cargo build --release --locked -p forge-cli --target "$TARGET"
bin="target/$TARGET/release/forge"
test -x "$bin"

host="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$TARGET" = "$host" ]; then
  observed="$("$bin" --version | awk '{print $2}')"
  test "$observed" = "$VERSION"
elif [[ "$TARGET" == *-apple-darwin ]]; then
  test "$(lipo -archs "$bin")" = "${TARGET%%-*}"
else
  file "$bin"
fi

name="forge-$VERSION-$TARGET"
mkdir -p out
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bin" "$stage/$name/forge"
cp LICENSE README.md CLI_ABI.md INVARIANTS.md "$stage/$name/"

# SOURCE_DATE_EPOCH equivalent: the committer time of the exact release commit.
# git renders it, so no platform date(1) dialect is involved.
stamp="$(TZ=UTC git show -s --date=format-local:%Y%m%d%H%M.%S --format=%cd "$COMMIT_SHA")"
test -n "$stamp"

chmod 0755 "$stage/$name" "$stage/$name/forge"
chmod 0644 "$stage/$name/CLI_ABI.md" "$stage/$name/INVARIANTS.md" \
  "$stage/$name/LICENSE" "$stage/$name/README.md"
TZ=UTC touch -t "$stamp" \
  "$stage/$name/CLI_ABI.md" "$stage/$name/INVARIANTS.md" "$stage/$name/LICENSE" \
  "$stage/$name/README.md" "$stage/$name/forge" "$stage/$name"

# GNU tar and bsdtar spell fixed ownership differently; both understand
# --numeric-owner, --no-recursion and --format=ustar.
ownership=(--numeric-owner)
if tar --version 2>/dev/null | head -n1 | grep -q 'GNU tar'; then
  ownership+=(--owner=0 --group=0)
else
  ownership+=(--uid 0 --gid 0 --uname '' --gname '')
fi

COPYFILE_DISABLE=1 tar --format=ustar --no-recursion "${ownership[@]}" \
  -cf - -C "$stage" \
  "$name" \
  "$name/CLI_ABI.md" \
  "$name/INVARIANTS.md" \
  "$name/LICENSE" \
  "$name/README.md" \
  "$name/forge" |
  gzip -9 -n >"out/$name.tar.gz"
test -s "out/$name.tar.gz"

{
  echo "artifact=$name.tar.gz"
  echo "version=$VERSION"
  echo "target=$TARGET"
  echo "commit=$COMMIT_SHA"
  echo "source_date_epoch=$(git show -s --format=%ct "$COMMIT_SHA")"
  echo "rustc=$(rustc --version)"
  echo "cargo=$(cargo --version)"
  echo "host=$host"
  echo "runner_image_os=${ImageOS:-unknown}"
  echo "runner_image_version=${ImageVersion:-unknown}"
  echo "kernel=$(uname -a)"
} >"out/$name.BUILD-INFO.txt"
