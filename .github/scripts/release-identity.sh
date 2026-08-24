#!/usr/bin/env bash
set -euo pipefail

: "${REF_TYPE:?REF_TYPE is required}"
: "${REF_NAME:?REF_NAME is required}"
: "${EVENT_NAME:?EVENT_NAME is required}"
: "${COMMIT_SHA:?COMMIT_SHA is required}"
: "${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
test "$(git rev-parse HEAD)" = "$COMMIT_SHA"

tag=''
publish=false
prerelease=false
if [ "$REF_TYPE" = tag ]; then
  tag="$REF_NAME"
  if ! printf '%s' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$'; then
    echo "::error title=Invalid release tag::$tag is not canonical SemVer vMAJOR.MINOR.PATCH[-PRERELEASE]"
    exit 1
  fi
  report="$(scripts/verify-tag-version.sh "$tag")"
  publish=true
  case "$tag" in *-*) prerelease=true ;; esac

  # A tag is not a review. Its commit must already be reachable from main.
  git fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main
  if ! git merge-base --is-ancestor "$COMMIT_SHA" refs/remotes/origin/main; then
    echo "::error title=Release commit is not on main::$COMMIT_SHA is not reachable from origin/main"
    exit 1
  fi
else
  report="$(scripts/verify-tag-version.sh)"
fi

printf '%s\n' "$report"
version="$(printf '%s\n' "$report" | sed -n 's/^workspace_version=//p')"
test -n "$version"
cargo_version="$(sed -n '/^\[workspace.package\]/,/^\[/ s/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
if [ "$cargo_version" != "$version" ]; then
  echo "::error title=Version source mismatch::Cargo.toml=$cargo_version verifier=$version"
  exit 1
fi

{
  echo "version=$version"
  echo "tag=$tag"
  echo "publish=$publish"
  echo "prerelease=$prerelease"
} >>"$GITHUB_OUTPUT"
printf 'version=%s tag=%s publish=%s event=%s\n' "$version" "${tag:-<none>}" "$publish" "$EVENT_NAME"
