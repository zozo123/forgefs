#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
mkdir -p labelled
if [ -d evidence ]; then
  for file in evidence/*; do
    [ -f "$file" ] || continue
    base="$(basename "$file")"
    stem="${base%.*}"
    extension="${base##*.}"
    cp "$file" "labelled/$stem-$TARGET.$extension"
  done
fi
