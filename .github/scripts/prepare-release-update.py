#!/usr/bin/env python3
"""Update the single workspace version source without touching dependencies."""

from __future__ import annotations

import os
import re
import tomllib
from pathlib import Path


next_version = os.environ["NEXT"]
if re.fullmatch(r"\d+\.\d+\.\d+", next_version) is None:
    raise SystemExit(f"NEXT must be stable SemVer, got {next_version!r}")

path = Path("Cargo.toml")
if path.is_symlink():
    raise SystemExit("Cargo.toml must not be a symlink")
text = path.read_text(encoding="utf-8")
try:
    current = tomllib.loads(text)["workspace"]["package"]["version"]
except (KeyError, TypeError, tomllib.TOMLDecodeError) as exception:
    raise SystemExit(f"cannot read workspace.package.version: {exception}") from exception
if re.fullmatch(r"\d+\.\d+\.\d+", current) is None:
    raise SystemExit(f"current workspace version must be stable SemVer, got {current!r}")

lines = text.splitlines(keepends=True)
section_starts = [
    index for index, line in enumerate(lines) if line.strip() == "[workspace.package]"
]
if len(section_starts) != 1:
    raise SystemExit("expected exactly one [workspace.package] section")
start = section_starts[0] + 1
end = next(
    (
        index
        for index in range(start, len(lines))
        if re.fullmatch(r"\s*\[[^]]+\]\s*", lines[index].rstrip("\r\n"))
    ),
    len(lines),
)
version_line = re.compile(r'(?P<prefix>\s*version\s*=\s*")(?P<value>[^"]+)(?P<suffix>"\s*)')
matches: list[tuple[int, re.Match[str]]] = []
for index in range(start, end):
    match = version_line.fullmatch(lines[index].rstrip("\r\n"))
    if match is not None:
        matches.append((index, match))
if len(matches) != 1:
    raise SystemExit("failed to update exactly one workspace.package.version")

index, match = matches[0]
if match.group("value") != current:
    raise SystemExit("TOML parser and workspace version source disagree")
ending = lines[index][len(lines[index].rstrip("\r\n")) :]
lines[index] = match.group("prefix") + next_version + match.group("suffix") + ending
updated = "".join(lines)
try:
    observed = tomllib.loads(updated)["workspace"]["package"]["version"]
except (KeyError, TypeError, tomllib.TOMLDecodeError) as exception:
    raise SystemExit(f"updated Cargo.toml is invalid: {exception}") from exception
if observed != next_version:
    raise SystemExit(f"updated version mismatch: expected {next_version}, got {observed}")
path.write_text(updated, encoding="utf-8")
