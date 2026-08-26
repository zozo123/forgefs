#!/usr/bin/env python3
"""Enforce ForgeFS's restricted, dependency-free Actions trust grammar.

The checker intentionally accepts only block-style YAML for security-sensitive
workflow structure. Unsupported flow collections and YAML indirection fail
closed instead of being partially interpreted by a line-oriented policy.
"""

from __future__ import annotations

import re
import stat
import sys
from pathlib import Path


WORKFLOW_DIR = Path(".github/workflows")
MAX_WORKFLOW_BYTES = 256 * 1024
MAX_WORKFLOWS = 64
REMOTE_ACTION = re.compile(
    r"^[^/@\s]+/[^/@\s]+(?:/[^@\s]+)*@([0-9a-fA-F]{40})$"
)
MAPPING = re.compile(
    r"^(?P<indent>\s*)(?:(?P<dash>-)(?P<dash_space>\s+))?"
    r"(?P<key>\"[^\"]+\"|'[^']+'|[A-Za-z0-9_.-]+)\s*:\s*(?P<value>.*)$"
)
LIST_SCALAR = re.compile(r"^\s*-\s*(?P<value>[^:]+?)\s*$")
STEP = re.compile(r"^(\s*)-\s+")
FLOW_NODE = re.compile(r"^\s*(?:-\s*)?(?:\[|\{(?!\{))")
YAML_MERGE_KEY = re.compile(r"^\s*(?:-\s*)?<<\s*:")
EXPLICIT_KEY = re.compile(r"^\s*(?:-\s*)?[?:](?:\s|$)")
ESCAPED_KEY = re.compile(r'^\s*(?:-\s*)?"[^"\\]*(?:\\.[^"\\]*)+"\s*:')
DOWNLOAD_COMMAND = re.compile(r"\b(?:curl|wget)\b", re.IGNORECASE)
BLOCK_SCALAR = re.compile(r"^[|>](?:[-+]?[1-9]?|[1-9][-+]?)$")
ALLOWED_REMOTE_ACTIONS = frozenset(
    {
        "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
        "actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8",
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
        "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
        "dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c",
        "dtolnay/rust-toolchain@7c8d7d138f5c09cef361f8214cf96882cd029cdb",
    }
)
ALLOWED_RUNNERS = frozenset(
    {"macos-15", "macos-15-intel", "ubuntu-24.04", "ubuntu-24.04-arm"}
)
MATRIX_RUNNER = "${{ matrix.os }}"
FORBIDDEN_STRUCTURE_KEYS = frozenset(
    {"container", "services", "shell", "working-directory"}
)
FORBIDDEN_ENV_KEYS = frozenset(
    {
        "bash_env",
        "dyld_insert_libraries",
        "ld_preload",
        "path",
        "pythonpath",
        "rustc_wrapper",
        "shellopts",
    }
)
ALLOWED_RUN_COMMANDS = frozenset(
    {
        ".github/scripts/prepare-release-gate.sh",
        ".github/scripts/prepare-release-apply.sh",
        ".github/scripts/prepare-release-package.sh",
        ".github/scripts/prepare-release-pr.sh",
        ".github/scripts/prepare-release-update.sh",
        ".github/scripts/release-assemble.sh",
        ".github/scripts/release-build.sh",
        ".github/scripts/release-e2e-artifact.sh",
        ".github/scripts/release-identity.sh",
        ".github/scripts/release-label-evidence.sh",
        ".github/scripts/release-publish.sh",
        ".github/scripts/release-reproduce.sh",
        ".github/scripts/release-sbom.sh",
        ".github/scripts/release-verify-payload.sh payload",
        "cargo +nightly fuzz run cap_token -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo +nightly fuzz run object_decode -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo +nightly fuzz run protocol_frame -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo +nightly fuzz run ref_name -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo +nightly fuzz run tar_roundtrip -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo +nightly fuzz run tree_name -- -max_total_time=60 -rss_limit_mb=2048",
        "cargo audit --deny warnings",
        "cargo build --locked -p forge-cli",
        "cargo check --manifest-path fuzz/Cargo.toml --bins",
        "cargo check --workspace --all-targets --locked",
        "cargo clippy --workspace --all-targets --locked -- -D warnings",
        "cargo deny --locked check advisories bans licenses sources",
        "cargo fmt --all -- --check",
        "cargo install cargo-audit --version 0.22.2 --locked",
        "cargo install cargo-cyclonedx --version 0.5.9 --locked",
        "cargo install cargo-deny --version 0.20.2 --locked",
        "cargo install cargo-fuzz --version 0.13.2 --locked",
        "cargo metadata --locked --format-version 1 >/dev/null",
        "cargo run --locked -p forge-cli -- bench --agents 32 --shared 16 --workers 16",
        "cargo test --workspace --all-targets --locked",
        "git diff --exit-code -- Cargo.lock",
        "python3 .github/scripts/check-workflow-security.py",
        "python3 .github/scripts/prepare-release-version.py",
        "python3 .github/scripts/test-release-tooling.py",
        "python3 .github/scripts/test-workflow-security.py",
        "scripts/cli-abi-conformance.sh target/debug/forge",
        "scripts/release-gate.sh target/debug/forge",
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


def is_complete_quoted_scalar(value: str) -> bool:
    """Return whether a quoted scalar opens and closes on this source line."""
    value = value.strip()
    if not value or value[0] not in {'"', "'"}:
        return True
    quote = value[0]
    escaped = False
    index = 1
    while index < len(value):
        char = value[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                return index == len(value) - 1
        elif char == quote:
            if index + 1 < len(value) and value[index + 1] == quote:
                index += 1
            else:
                return index == len(value) - 1
        index += 1
    return False


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
    """Detect a tag token at a YAML node boundary, not `!` inside a scalar."""
    candidate = line.lstrip()
    if candidate.startswith("-"):
        candidate = candidate[1:].lstrip()
    if candidate.startswith("!"):
        return True
    item = mapping(line)
    return item is not None and item[2].lstrip().startswith("!")


def check_run_command(
    command: str, errors: list[str], path: Path, line: int
) -> None:
    """Allow only whole, audited commands; do not approximate a shell parser."""
    command = scalar(command)
    deescaped = re.sub(r"\\(.)", r"\1", command)
    if DOWNLOAD_COMMAND.search(deescaped):
        error(
            errors,
            path,
            line,
            "direct curl/wget commands are forbidden in workflow run steps",
        )
    normalized = " ".join(command.split())
    if normalized not in ALLOWED_RUN_COMMANDS:
        error(errors, path, line, "workflow run command is not allowlisted")


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
    key_indent = len(match.group("indent"))
    if match.group("dash"):
        key_indent += 1 + len(match.group("dash_space"))
    return (
        key_indent,
        scalar(match.group("key")).lower(),
        match.group("value").strip(),
    )


def parent_mapping_key(lines: list[str], index: int, child_indent: int) -> str | None:
    """Return the nearest enclosing mapping key for a source line."""
    for candidate in range(index - 1, -1, -1):
        item = mapping(lines[candidate])
        if item is not None and item[0] < child_indent:
            return item[1]
    return None


def check_runner_list(
    lines: list[str],
    start: int,
    parent_indent: int,
    errors: list[str],
    path: Path,
) -> int:
    """Validate a block-style matrix `os` list and return its value count."""
    count = 0
    list_indent: int | None = None
    for candidate in range(start + 1, len(lines)):
        line = lines[candidate]
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        if indent <= parent_indent:
            break
        listed = LIST_SCALAR.match(line)
        if listed is None:
            error(
                errors,
                path,
                candidate + 1,
                "matrix os must be a flat block list of fixed runners",
            )
            continue
        if list_indent is None:
            list_indent = indent
        if indent != list_indent:
            error(
                errors,
                path,
                candidate + 1,
                "matrix os runner has unexpected nesting",
            )
            continue
        runner = scalar(listed.group("value"))
        if runner not in ALLOWED_RUNNERS:
            error(errors, path, candidate + 1, "runner label is not allowlisted")
        count += 1
    if count == 0:
        error(errors, path, start + 1, "matrix os must declare fixed runners")
    return count


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
    for position, line in enumerate(step[1:], 1):
        item = mapping(line)
        if item is None:
            continue
        with_indent, key, _ = item
        if key != "with" or with_indent <= step_indent:
            continue
        children: list[tuple[int, str, str]] = []
        for child_line in step[position + 1 :]:
            child = mapping(child_line)
            if child is None:
                continue
            if child[0] <= with_indent:
                break
            children.append(child)
        if not children:
            return False
        child_indent = min(child[0] for child in children)
        return any(
            key == "persist-credentials"
            and indent == child_indent
            and yaml_bool(value) is False
            for indent, key, value in children
        )
    return False


def check_workflow(path: Path, errors: list[str]) -> None:
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            error(errors, path, 1, "workflow must be a regular file, not a symlink")
            return
        if metadata.st_size > MAX_WORKFLOW_BYTES:
            error(
                errors,
                path,
                1,
                f"workflow exceeds {MAX_WORKFLOW_BYTES}-byte policy limit",
            )
            return
        raw_lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exception:
        error(errors, path, 1, f"cannot read workflow safely: {exception}")
        return
    lines = [strip_yaml_comment(line) for line in raw_lines]
    top_level_permissions = False
    block_scalar_indent: int | None = None
    block_scalar_key: str | None = None
    matrix_runner_lines: list[int] = []
    matrix_os_values = 0

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
        parent_key = parent_mapping_key(lines, index - 1, item_indent)
        if not is_complete_quoted_scalar(value):
            error(
                errors,
                path,
                index,
                "multiline or compound quoted YAML scalars are forbidden",
            )
        if BLOCK_SCALAR.fullmatch(value):
            block_scalar_indent = item_indent
            block_scalar_key = key
            if key == "run":
                error(
                    errors,
                    path,
                    index,
                    "block-scalar run steps are forbidden; use an allowlisted inline command",
                )
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
        if key in FORBIDDEN_STRUCTURE_KEYS:
            error(
                errors,
                path,
                index,
                f"{key} is forbidden by the restricted execution grammar",
            )
        if parent_key == "env" and key in FORBIDDEN_ENV_KEYS:
            error(
                errors,
                path,
                index,
                f"environment key {key} can alter command execution",
            )
        if key == "matrix" and value:
            error(
                errors,
                path,
                index,
                "dynamic or scalar matrices are forbidden; declare a block matrix",
            )
        if key == "include" and parent_key == "matrix" and value:
            error(
                errors,
                path,
                index,
                "dynamic matrix include values are forbidden",
            )
        if key == "runs-on":
            runner = scalar(value)
            if runner == MATRIX_RUNNER:
                matrix_runner_lines.append(index)
            elif runner not in ALLOWED_RUNNERS:
                error(errors, path, index, "runner label is not allowlisted")
        if key == "os" and parent_key in {"include", "matrix"}:
            if value:
                if scalar(value) not in ALLOWED_RUNNERS:
                    error(errors, path, index, "matrix runner label is not allowlisted")
                matrix_os_values += 1
            else:
                matrix_os_values += check_runner_list(
                    lines, index - 1, item_indent, errors, path
                )

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
            error(errors, path, index, "Docker actions are not allowlisted")
            continue
        if not REMOTE_ACTION.fullmatch(target):
            error(errors, path, index, "pin remote actions to a full 40-hex commit SHA")
            continue
        if target not in ALLOWED_REMOTE_ACTIONS:
            error(errors, path, index, "remote action is not allowlisted")
            continue
        if target.lower().startswith("actions/checkout@"):
            step, step_indent = checkout_step(lines, index - 1, item_indent)
            if not checkout_disables_credentials(step, step_indent):
                error(errors, path, index, "set checkout persist-credentials: false")

    if not top_level_permissions:
        error(errors, path, 1, "declare least-privilege top-level permissions")
    if matrix_runner_lines and matrix_os_values == 0:
        for line in matrix_runner_lines:
            error(
                errors,
                path,
                line,
                "matrix.os runner requires an explicit fixed os matrix",
            )


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if len(arguments) > 1:
        print("usage: check-workflow-security.py [WORKFLOW_DIR]", file=sys.stderr)
        return 2
    workflow_dir = Path(arguments[0]) if arguments else WORKFLOW_DIR
    errors: list[str] = []
    workflows = sorted((*workflow_dir.glob("*.yml"), *workflow_dir.glob("*.yaml")))
    if not workflows:
        print("no workflows found", file=sys.stderr)
        return 1
    if len(workflows) > MAX_WORKFLOWS:
        print(f"too many workflows: {len(workflows)} > {MAX_WORKFLOWS}", file=sys.stderr)
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
