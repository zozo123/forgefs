#!/usr/bin/env bash
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
COPYFILE_DISABLE=1 tar -czf "out/$name.tar.gz" -C "$stage" "$name"
{
  echo "artifact=$name.tar.gz"
  echo "version=$VERSION"
  echo "target=$TARGET"
  echo "commit=$COMMIT_SHA"
  echo "rustc=$(rustc --version)"
  echo "cargo=$(cargo --version)"
  echo "host=$host"
  echo "runner_image_os=${ImageOS:-unknown}"
  echo "runner_image_version=${ImageVersion:-unknown}"
  echo "kernel=$(uname -a)"
} >"out/$name.BUILD-INFO.txt"
