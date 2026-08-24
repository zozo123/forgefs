#!/usr/bin/env bash
set -euo pipefail

: "${NEXT:?NEXT is required}"
scripts/verify-tag-version.sh "v$NEXT" >/dev/null
git diff --check
test -z "$(git diff --summary -- Cargo.toml Cargo.lock)"
test -n "$(git diff --name-only)"
while IFS= read -r changed; do
  case "$changed" in
    Cargo.toml | Cargo.lock) ;;
    *)
      echo "::error title=Unexpected release-preparation change::$changed"
      exit 1
      ;;
  esac
done < <(git diff --name-only)

mkdir release-preparation
git diff --binary -- Cargo.toml Cargo.lock >release-preparation/release.patch
test -s release-preparation/release.patch
printf '%s\n' "$NEXT" >release-preparation/next-version
