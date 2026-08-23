#!/usr/bin/env python3
"""Enforce the repository's dependency-free GitHub Actions trust floor."""

from __future__ import annotations

import re
import sys
from pathlib import Path


WORKFLOW_DIR = Path(".github/workflows")
REMOTE_ACTION = re.compile(r"^[^/@\s]+/[^@\s]+@([0-9a-fA-F]{40})$")
USES = re.compile(r"^(\s*)-?\s*uses:\s*['\"]?([^'\"\s#]+)")
STEP = re.compile(r"^(\s*)-\s+(?:name:|uses:|run:)")
CARGO_INSTALL = re.compile(r"\bcargo\s+install\s+([A-Za-z0-9_-]+)\b")


def error(errors: list[str], path: Path, line: int, message: str) -> None:
    errors.append(f"{path}:{line}: {message}")


def checkout_step(lines: list[str], index: int, indent: int) -> str:
    end = len(lines)
    for candidate in range(index + 1, len(lines)):
        match = STEP.match(lines[candidate])
        if match and len(match.group(1)) == indent:
            end = candidate
            break
    return "\n".join(lines[index:end])


def check_workflow(path: Path, errors: list[str]) -> None:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()

    if not re.search(r"(?m)^permissions:\s*$", text):
        error(errors, path, 1, "declare least-privilege top-level permissions")

    forbidden = {
        "pull_request_target:": "pull_request_target executes privileged base code",
        "permissions: write-all": "write-all permissions are forbidden",
        "persist-credentials: true": "checkout credentials must not persist",
    }
    for index, line in enumerate(lines, 1):
        normalized = line.strip().lower()
        for marker, message in forbidden.items():
            if normalized == marker:
                error(errors, path, index, message)

        uses = USES.match(line)
        if uses:
            target = uses.group(2)
            if target.startswith("./"):
                continue
            if target.startswith("docker://"):
                if "@sha256:" not in target:
                    error(errors, path, index, "pin Docker actions by sha256 digest")
                continue
            if not REMOTE_ACTION.fullmatch(target):
                error(errors, path, index, "pin remote actions to a full 40-hex commit SHA")
                continue
            if target.lower().startswith("actions/checkout@"):
                step = checkout_step(lines, index - 1, len(uses.group(1)))
                if not re.search(r"(?m)^\s+persist-credentials:\s*false\s*$", step):
                    error(errors, path, index, "set checkout persist-credentials: false")

        install = CARGO_INSTALL.search(line)
        if install and "--version" not in line:
            error(errors, path, index, f"pin cargo install {install.group(1)} with --version")

        if re.search(r"\b(?:curl|wget)\b.*\|\s*(?:ba)?sh\b", line):
            error(errors, path, index, "download-and-execute pipelines are forbidden")


def main() -> int:
    errors: list[str] = []
    workflows = sorted((*WORKFLOW_DIR.glob("*.yml"), *WORKFLOW_DIR.glob("*.yaml")))
    if not workflows:
        print("no workflows found", file=sys.stderr)
        return 1
    for workflow in workflows:
        check_workflow(workflow, errors)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"workflow trust policy: {len(workflows)} workflows passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
