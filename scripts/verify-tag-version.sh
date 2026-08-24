#!/usr/bin/env bash
# scripts/verify-tag-version.sh
#
# Refuse a release whose git tag disagrees with `workspace.package.version`.
# This is the single most common release footgun, so it is checked first, it is
# checked without needing a Rust toolchain, and it fails loudly.
#
# Usage:
#   scripts/verify-tag-version.sh [TAG] [REPO_ROOT]
#
# Run it before pushing a tag:
#   scripts/verify-tag-version.sh v0.1.0
#
# With no TAG it answers "what tag would this workspace need, and is the
# workspace self-consistent?" - the version check is skipped, the inheritance
# check still runs. Either way it prints one machine-readable line:
#
#   workspace_version=<version>
#
# The rule is exact string equality: the tag MUST be "v" followed by
# `workspace.package.version`, verbatim. That holds for prereleases too -
# v1.0.0-rc.1 requires version = "1.0.0-rc.1" in Cargo.toml. No normalisation,
# no "close enough", because a release that guesses is a release that lies.
#
# It additionally asserts that every workspace member inherits the workspace
# version, so a crate that quietly pins its own version cannot ship under a tag
# that does not describe it.
#
# Exit status:
#   0  the tag and the workspace version agree
#   1  they disagree, or a member does not inherit the workspace version
#   2  usage or harness error
set -euo pipefail

TAG="${1:-}"
ROOT="${2:-.}"

die() {
	printf 'verify-tag-version: %s\n' "$1" >&2
	exit 2
}

[ -f "$ROOT/Cargo.toml" ] || die "no Cargo.toml under $ROOT"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

VTV_TAG="$TAG" VTV_ROOT="$ROOT" python3 - <<'PY'
import glob
import os
import re
import sys

root = os.environ["VTV_ROOT"]
tag = os.environ["VTV_TAG"]
manifest = os.path.join(root, "Cargo.toml")


def table_of(path, wanted):
    """Return the raw key -> value lines of one top-level TOML table."""
    rows = {}
    current = None
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            stripped = line.split("#", 1)[0].strip()
            if not stripped:
                continue
            header = re.fullmatch(r"\[([^\[\]]+)\]", stripped)
            if header:
                current = header.group(1).strip()
                continue
            if current != wanted:
                continue
            key, _, value = stripped.partition("=")
            if not _:
                continue
            rows[key.strip()] = value.strip()
    return rows


workspace_package = table_of(manifest, "workspace.package")
raw = workspace_package.get("version")
if raw is None:
    print("verify-tag-version: Cargo.toml has no [workspace.package] version", file=sys.stderr)
    sys.exit(2)

version = raw.strip().strip('"').strip("'")
expected_tag = "v" + version

problems = []
if tag and tag != expected_tag:
    problems.append(
        "tag %r does not match workspace.package.version %r (expected tag %r)"
        % (tag, version, expected_tag)
    )

# Every member must inherit, or the tag does not describe what ships.
members = sorted(glob.glob(os.path.join(root, "crates", "*", "Cargo.toml")))
if not members:
    problems.append("found no crates/*/Cargo.toml to check for version inheritance")
for member in members:
    package = table_of(member, "package")
    own = package.get("version")
    inherits = package.get("version.workspace")
    if inherits is not None:
        if inherits.strip().lower() != "true":
            problems.append("%s: version.workspace is %s, expected true" % (member, inherits))
        continue
    if own is None:
        problems.append("%s: [package] declares no version at all" % member)
        continue
    if "workspace" not in own:
        problems.append(
            "%s: pins its own version %s instead of inheriting the workspace version"
            % (member, own)
        )

if problems:
    print("verify-tag-version: REFUSING RELEASE", file=sys.stderr)
    for problem in problems:
        print("  - " + problem, file=sys.stderr)
    print(file=sys.stderr)
    print(
        "  Fix Cargo.toml or delete and re-push the tag. Do not publish a tag\n"
        "  that does not name the version it contains.",
        file=sys.stderr,
    )
    sys.exit(1)

if tag:
    print("verify-tag-version: tag %s matches workspace.package.version %s" % (tag, version))
else:
    print("verify-tag-version: no tag given; this workspace requires tag %s" % expected_tag)
print("verify-tag-version: %d workspace member(s) inherit the workspace version" % len(members))
print("workspace_version=%s" % version)
PY
