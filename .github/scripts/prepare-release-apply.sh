#!/usr/bin/env bash
set -euo pipefail

: "${NEXT:?NEXT is required}"
artifact="release-preparation"
test -d "$artifact"
test ! -L "$artifact"
test "$(find "$artifact" -mindepth 1 -maxdepth 1 -type f | wc -l | tr -d ' ')" -eq 2
test -z "$(find "$artifact" -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
test "$(tr -d '\n' <"$artifact/next-version")" = "$NEXT"
test -s "$artifact/release.patch"

git apply --check "$artifact/release.patch"
git apply "$artifact/release.patch"
git diff --check
test -z "$(git diff --summary -- Cargo.toml Cargo.lock)"
test -n "$(git diff --name-only)"
while IFS= read -r changed; do
  case "$changed" in
    Cargo.toml | Cargo.lock) ;;
    *)
      echo "::error title=Unexpected release-preparation patch::$changed"
      exit 1
      ;;
  esac
done < <(git diff --name-only)
scripts/verify-tag-version.sh "v$NEXT" >/dev/null
