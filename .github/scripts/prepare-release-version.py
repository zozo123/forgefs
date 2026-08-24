#!/usr/bin/env python3
"""Compute the next stable workspace version for prepare-release."""

from __future__ import annotations

import os
import re
import tomllib
from pathlib import Path


text = Path("Cargo.toml").read_text(encoding="utf-8")
try:
    current = tomllib.loads(text)["workspace"]["package"]["version"]
except (KeyError, TypeError, tomllib.TOMLDecodeError) as exception:
    raise SystemExit(f"cannot read workspace.package.version: {exception}") from exception

match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", current)
if match is None:
    raise SystemExit(
        "workspace.package.version must be a plain stable SemVer before prepare-release"
    )

major, minor, patch = map(int, match.groups())
bump = os.environ["BUMP"]
if bump == "major":
    major, minor, patch = major + 1, 0, 0
elif bump == "minor":
    minor, patch = minor + 1, 0
elif bump == "patch":
    patch += 1
else:
    raise SystemExit(f"unsupported bump: {bump}")

with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
    output.write(f"current={current}\n")
    output.write(f"next={major}.{minor}.{patch}\n")
