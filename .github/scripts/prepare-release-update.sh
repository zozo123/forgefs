#!/usr/bin/env bash
set -euo pipefail

: "${NEXT:?NEXT is required}"
python3 .github/scripts/prepare-release-update.py

# Refresh only what the manifest version change requires. `cargo update` is
# intentionally forbidden here because it would mix dependency churn into the
# release-preparation PR.
cargo check --workspace --all-targets
scripts/verify-tag-version.sh "v$NEXT"

# Release preparation owns exactly these two files. Refuse to hide generated or
# incidental source changes in the release PR.
while IFS= read -r changed; do
  case "$changed" in
    Cargo.toml | Cargo.lock) ;;
    *)
      echo "::error title=Unexpected release-preparation change::$changed"
      exit 1
      ;;
  esac
done < <(git diff --name-only)
