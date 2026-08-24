#!/usr/bin/env bash
set -euo pipefail

: "${NEXT:?NEXT is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"
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

branch="release/v$NEXT"
gh auth setup-git
if git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
  echo "::error title=Release branch exists::$branch already exists"
  exit 1
fi

git config user.name github-actions[bot]
git config user.email 41898282+github-actions[bot]@users.noreply.github.com
git switch -c "$branch"
git add Cargo.toml Cargo.lock
git commit -m "release: prepare ForgeFS v$NEXT"
git push origin "HEAD:refs/heads/$branch"
gh pr create \
  --base main \
  --head "$branch" \
  --title "release: prepare ForgeFS v$NEXT" \
  --body "Bumps the single workspace version to **$NEXT** and refreshes Cargo.lock without updating unrelated dependencies. The prepare workflow already ran fmt/check/clippy/test, the CLI ABI table, and the end-to-end release gate on this exact tree. After merge, tag the merge commit **v$NEXT**; the tag workflow independently re-verifies version + main ancestry, rebuilds all targets, gates the exact packaged binaries, assembles one checksummed payload, attests it, and publishes it."
