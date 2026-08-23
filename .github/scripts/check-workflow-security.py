#!/usr/bin/env python3
"""Enforce ForgeFS's restricted, dependency-free Actions trust grammar.

The checker intentionally accepts only block-style YAML for security-sensitive
workflow structure. Unsupported flow collections and YAML indirection fail
closed instead of being partially interpreted by a line-oriented policy.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


WORKFLOW_DIR = Path(".github/workflows")
REMOTE_ACTION = re.compile(
    r"^[^/@\s]+/[^/@\s]+(?:/[^@\s]+)*@([0-9a-fA-F]{40})$"
)
DOCKER_ACTION = re.compile(r"^docker://[^@\s]+@sha256:[0-9a-fA-F]{64}$")
MAPPING = re.compile(
    r"^(?P<indent>\s*)(?:-\s*)?"
    r"(?P<key>\"[^\"]+\"|'[^']+'|[A-Za-z0-9_.-]+)\s*:\s*(?P<value>.*)$"
)
LIST_SCALAR = re.compile(r"^\s*-\s*(?P<value>[^:]+?)\s*$")
STEP = re.compile(r"^(\s*)-\s+")
FLOW_NODE = re.compile(r"^\s*(?:-\s*)?(?:\[|\{(?!\{))")
YAML_MERGE_KEY = re.compile(r"^\s*(?:-\s*)?<<\s*:")
EXPLICIT_KEY = re.compile(r"^\s*(?:-\s*)?[?:](?:\s|$)")
ESCAPED_KEY = re.compile(r'^\s*(?:-\s*)?"[^"\\]*(?:\\.[^"\\]*)+"\s*:')
CARGO_INSTALL = re.compile(r"\bcargo\s+install\s+([A-Za-z0-9_-]+)\b")
DOWNLOAD_COMMAND = re.compile(r"\b(?:curl|wget)\b", re.IGNORECASE)
BLOCK_SCALAR = re.compile(r"^[|>](?:[-+]?[1-9]?|[1-9][-+]?)$")
ALLOWED_CARGO_INSTALLS = frozenset(
    {
        "cargo install cargo-audit --version 0.22.2 --locked",
        "cargo install cargo-fuzz --version 0.13.2 --locked",
    }
)


def error(errors: list[str], path: Path, line: int, message: str) -> None:
    errors.append(f"{path}:{line}: {message}")


def strip_yaml_comment(line: str) -> str:
    """Remove a YAML comment without treating a quoted # as a comment."""
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif quote == "'":
            if char == quote:
                if index + 1 < len(line) and line[index + 1] == quote:
                    index += 1
                else:
                    quote = None
        elif char in {'"', "'"}:
            quote = char
        elif char == "#" and (index == 0 or line[index - 1].isspace()):
            return line[:index].rstrip()
        index += 1
    return line.rstrip()


def scalar(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "'"}:
        value = value[1:-1]
    return value


def has_flow_yaml(line: str) -> bool:
    if FLOW_NODE.search(line):
        return True
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif quote == "'":
            if char == quote:
                if index + 1 < len(line) and line[index + 1] == quote:
                    index += 1
                else:
                    quote = None
        elif char in {'"', "'"}:
            quote = char
        elif char == ":":
            candidate = index + 1
            while candidate < len(line) and line[candidate].isspace():
                candidate += 1
            if candidate < len(line) and line[candidate] == "[":
                return True
            if (
                candidate < len(line)
                and line[candidate] == "{"
                and not line[candidate : candidate + 2] == "{{"
            ):
                return True
        index += 1
    return False


def has_yaml_indirection(line: str) -> bool:
    if YAML_MERGE_KEY.search(line):
        return True
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif quote == "'":
            if char == quote:
                if index + 1 < len(line) and line[index + 1] == quote:
                    index += 1
                else:
                    quote = None
        elif char in {'"', "'"}:
            quote = char
        elif char in {"&", "*"}:
            previous = line[index - 1] if index else " "
            following = line[index + 1] if index + 1 < len(line) else " "
            if (
                (previous.isspace() or previous in "[{,?:-")
                and not following.isspace()
                and following != char
            ):
                return True
        index += 1
    return False


def has_yaml_tag(line: str) -> bool:
    """Detect YAML tags while ignoring quoted text and GitHub expressions."""
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(line):
        if quote is None and line.startswith("${{", index):
            end = line.find("}}", index + 3)
            if end == -1:
                return False
            index = end + 2
            continue
        char = line[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif quote == "'":
            if char == quote:
                if index + 1 < len(line) and line[index + 1] == quote:
                    index += 1
                else:
                    quote = None
        elif char in {'"', "'"}:
            quote = char
        elif char == "!":
            previous = line[index - 1] if index else " "
            if previous.isspace() or previous in "[{,:?-":
                return True
        index += 1
    return False


def check_run_command(
    command: str, errors: list[str], path: Path, line: int
) -> None:
    """Enforce the deliberately small shell dependency-install surface."""
    command = scalar(command)
    if "\\" in command:
        error(
            errors,
            path,
            line,
            "backslash escapes and continuations are forbidden in workflow run steps",
        )
    deescaped = re.sub(r"\\(.)", r"\1", command)
    if DOWNLOAD_COMMAND.search(deescaped):
        error(
            errors,
            path,
            line,
            "direct curl/wget commands are forbidden in workflow run steps",
        )
    install = CARGO_INSTALL.search(deescaped)
    if install:
        normalized = " ".join(deescaped.split())
        if normalized not in ALLOWED_CARGO_INSTALLS:
            error(
                errors,
                path,
                line,
                f"cargo install {install.group(1)} must match an audited exact command",
            )


def yaml_bool(value: str) -> bool | None:
    normalized = scalar(value).lower()
    if normalized in {"true", "yes", "on"}:
        return True
    if normalized in {"false", "no", "off"}:
        return False
    return None


def mapping(line: str) -> tuple[int, str, str] | None:
    match = MAPPING.match(line)
    if not match:
        return None
    return (
        len(match.group("indent")),
        scalar(match.group("key")).lower(),
        match.group("value").strip(),
    )


def checkout_step(lines: list[str], index: int, uses_indent: int) -> tuple[list[str], int]:
    start = index
    step_indent = uses_indent
    current = STEP.match(lines[index])
    if current:
        step_indent = len(current.group(1))
    else:
        for candidate in range(index - 1, -1, -1):
            match = STEP.match(lines[candidate])
            if match and len(match.group(1)) < uses_indent:
                start = candidate
                step_indent = len(match.group(1))
                break
    end = len(lines)
    for candidate in range(start + 1, len(lines)):
        match = STEP.match(lines[candidate])
        if match and len(match.group(1)) == step_indent:
            end = candidate
            break
    return lines[start:end], step_indent


def checkout_disables_credentials(step: list[str], step_indent: int) -> bool:
    with_indent: int | None = None
    for line in step[1:]:
        item = mapping(line)
        if item is None:
            continue
        indent, key, value = item
        if indent <= step_indent:
            with_indent = None
        if key == "with":
            with_indent = indent
            continue
        if with_indent is not None and indent <= with_indent:
            with_indent = None
        if (
            with_indent is not None
            and key == "persist-credentials"
            and indent == with_indent + 2
        ):
            return yaml_bool(value) is False
    return False


def check_workflow(path: Path, errors: list[str]) -> None:
    raw_lines = path.read_text(encoding="utf-8").splitlines()
    lines = [strip_yaml_comment(line) for line in raw_lines]
    top_level_permissions = False
    block_scalar_indent: int | None = None
    block_scalar_key: str | None = None

    for index, line in enumerate(lines, 1):
        stripped = line.strip()
        if not stripped:
            continue
        indent = len(line) - len(line.lstrip())
        in_block_scalar = (
            block_scalar_indent is not None and indent > block_scalar_indent
        )
        if block_scalar_indent is not None and not in_block_scalar:
            block_scalar_indent = None
            block_scalar_key = None

        if in_block_scalar:
            if block_scalar_key == "run":
                check_run_command(line.strip(), errors, path, index)
            continue
        if has_flow_yaml(line):
            error(
                errors,
                path,
                index,
                "flow-style YAML is forbidden; use auditable block structure",
            )
        if has_yaml_indirection(line):
            error(
                errors,
                path,
                index,
                "YAML anchors, aliases, and merge keys are forbidden",
            )
        if has_yaml_tag(line):
            error(
                errors,
                path,
                index,
                "YAML tags are forbidden",
            )
        if EXPLICIT_KEY.search(line) or ESCAPED_KEY.search(line):
            error(
                errors,
                path,
                index,
                "explicit or escaped YAML mapping keys are forbidden",
            )

        item = mapping(line)
        if item is None:
            listed = LIST_SCALAR.match(line)
            if listed and scalar(listed.group("value")).lower() == "pull_request_target":
                error(
                    errors,
                    path,
                    index,
                    "pull_request_target executes privileged base code",
                )
            continue

        item_indent, key, value = item
        if BLOCK_SCALAR.fullmatch(value):
            # In a compact sequence mapping (`- name: |`) sibling keys are two
            # columns deeper than the dash.  They are not scalar content.
            block_scalar_indent = item_indent + 2 if STEP.match(line) else item_indent
            block_scalar_key = key
        elif key == "run":
            check_run_command(value, errors, path, index)
        if item_indent == 0 and key == "permissions":
            top_level_permissions = True
        trigger_value = scalar(value).lower()
        if key == "pull_request_target" or (
            key == "on"
            and trigger_value in {"pull_request_target", "pull_request_target:"}
        ):
            error(
                errors,
                path,
                index,
                "pull_request_target executes privileged base code",
            )
        if key == "permissions" and scalar(value).lower() == "write-all":
            error(errors, path, index, "write-all permissions are forbidden")
        if key == "persist-credentials" and yaml_bool(value) is not False:
            error(errors, path, index, "checkout credentials must not persist")

        if key != "uses":
            continue
        target = scalar(value)
        if target.startswith("./"):
            error(
                errors,
                path,
                index,
                "local actions are forbidden; use audited repository scripts or pinned remote actions",
            )
            continue
        if target.startswith("docker://"):
            if not DOCKER_ACTION.fullmatch(target):
                error(errors, path, index, "pin Docker actions by sha256 digest")
            continue
        if not REMOTE_ACTION.fullmatch(target):
            error(errors, path, index, "pin remote actions to a full 40-hex commit SHA")
            continue
        if target.lower().startswith("actions/checkout@"):
            step, step_indent = checkout_step(lines, index - 1, item_indent)
            if not checkout_disables_credentials(step, step_indent):
                error(errors, path, index, "set checkout persist-credentials: false")

    if not top_level_permissions:
        error(errors, path, 1, "declare least-privilege top-level permissions")


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
