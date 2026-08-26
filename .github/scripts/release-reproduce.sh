#!/usr/bin/env bash
# Prove the release packaging is reproducible instead of asserting it.
#
# Builds and packages the exact release commit twice: once in the checkout,
# once in a second clean git worktree at a different absolute path with its
# own empty target directory. Every file the packaging step produces must be
# byte-identical. A difference fails the release rather than being explained
# away in a document.
#
# DESTRUCTIVE, and deliberately so: a second build is only evidence if the
# first one left nothing behind, so this removes ./out and ./target before it
# starts. It is meant for a fresh CI checkout, not for a working tree you care
# about.
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"
: "${COMMIT_SHA:?COMMIT_SHA is required}"
test "$(git rev-parse HEAD)" = "$COMMIT_SHA"

root="$(pwd)"
name="forge-$VERSION-$TARGET"

second_parent="$(mktemp -d)"
second="$second_parent/rebuild"
cleanup() {
  cd "$root"
  git worktree remove --force "$second" >/dev/null 2>&1 || true
  rm -rf "$second_parent"
}
trap cleanup EXIT

rm -rf out target
"$root/.github/scripts/release-build.sh"
"$root/.github/scripts/release-sbom.sh"

git worktree add --detach "$second" "$COMMIT_SHA" >/dev/null
cd "$second"
"$root/.github/scripts/release-build.sh"
"$root/.github/scripts/release-sbom.sh"
cd "$root"

mkdir -p reproduce
report="reproduce/$name.REPRODUCE.txt"
{
  echo "target=$TARGET"
  echo "version=$VERSION"
  echo "commit=$COMMIT_SHA"
  echo "rustc=$(rustc --version)"
  echo "method=two clean packaging runs of the same commit in separate trees at different absolute paths"
} >"$report"

status=0
count=0
for file in out/*; do
  base="$(basename "$file")"
  other="$second/out/$base"
  if [ ! -f "$other" ]; then
    echo "::error title=Reproducibility gap::$base missing from the rebuild"
    status=1
    continue
  fi
  first_sum="$(sha256sum "$file" | awk '{print $1}')"
  second_sum="$(sha256sum "$other" | awk '{print $1}')"
  count=$((count + 1))
  if [ "$first_sum" = "$second_sum" ]; then
    printf 'identical %s %s\n' "$first_sum" "$base" >>"$report"
  else
    printf 'DIFFERENT %s %s %s\n' "$first_sum" "$second_sum" "$base" >>"$report"
    echo "::error title=Reproducibility failure::$base differs between two builds of $COMMIT_SHA"
    status=1
  fi
done

for file in "$second"/out/*; do
  base="$(basename "$file")"
  if [ ! -f "out/$base" ]; then
    echo "::error title=Reproducibility gap::$base only produced by the rebuild"
    status=1
  fi
done

if [ "$count" -lt 3 ]; then
  echo "::error title=Reproducibility check is vacuous::only $count packaged files compared"
  status=1
fi
printf 'compared=%s\n' "$count" >>"$report"
printf 'result=%s\n' "$([ "$status" -eq 0 ] && echo reproducible || echo not-reproducible)" >>"$report"
cat "$report"
exit "$status"
