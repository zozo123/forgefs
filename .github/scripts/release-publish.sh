#!/usr/bin/env bash
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GH_REPO:?GH_REPO is required}"
: "${TAG:?TAG is required}"
: "${VERSION:?VERSION is required}"
: "${PRERELEASE:?PRERELEASE is required}"
: "${COMMIT_SHA:?COMMIT_SHA is required}"

scripts/verify-tag-version.sh "$TAG" >/dev/null
test "$(git rev-parse HEAD)" = "$COMMIT_SHA"
.github/scripts/release-verify-payload.sh payload
cd payload
if gh release view "$TAG" --repo "$GH_REPO" >/dev/null 2>&1; then
  echo "::error title=Release already exists::$TAG already exists; refusing to mutate an existing release"
  exit 1
fi

notes="$(mktemp)"
trap 'rm -f "$notes"' EXIT
{
  printf '# ForgeFS %s\n\n' "$TAG"
  printf 'Commit `%s`. Workspace version `%s`.\n\n' "$COMMIT_SHA" "$VERSION"
  printf '%s\n\n' 'This release was built with the locked dependency graph, gated on Linux and macOS, tested from the exact packaged binaries, checksummed as one immutable payload, and attested before publication.'
  printf '%s\n\n' 'Verify downloaded assets with:'
  printf '`sha256sum -c SHA256SUMS`\n\n'
  printf '%s\n' 'Evidence files (gate summary, full fsck, CLI ABI, seal attestation, environment line, and build info) are attached alongside the binaries.'
} >"$notes"

arguments=(--title "ForgeFS $TAG" --notes-file "$notes" --verify-tag)
if [ "$PRERELEASE" = true ]; then
  arguments+=(--prerelease)
fi
gh release create "$TAG" --repo "$GH_REPO" "${arguments[@]}" ./*
