#!/usr/bin/env python3
"""R1 (#51): the CI graph, the admission script, the ruleset and CODEOWNERS agree.

This is defense-in-depth that runs in PR-controlled CI. The binding control is
the protect-main ruleset (applied from .github/rulesets/protect-main.json),
which names every correctness context individually and pins it to the GitHub
Actions app. A PR that renames, removes or skips a gate therefore leaves a
required context unreported and cannot merge, whatever this script says.

What this script catches is drift: a PR that changes one of the four sources
of truth without the others, so the reviewed ruleset file stops describing the
CI graph that actually runs.
"""

from __future__ import annotations

import importlib.util
import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CI_WORKFLOW = Path(".github/workflows/ci.yml")
RULESET = Path(".github/rulesets/protect-main.json")
TAG_RULESET = Path(".github/rulesets/release-tags.json")
CODEOWNERS = Path(".github/CODEOWNERS")
ADMISSION_SCRIPT = ROOT / ".github" / "scripts" / "check-ci-admission.py"
ADMISSION_JOB = "admission"
ADMISSION_RUN = "python3 .github/scripts/check-ci-admission.py"
ADMISSION_NEEDS_ENV = "FORGE_CI_NEEDS: ${{ toJSON(needs) }}"
GITHUB_ACTIONS_APP_ID = 15368
TRUST_ROOT_OWNER_PATH = "/.github/"
JOB_KEY = re.compile(r"^  (?P<id>[A-Za-z0-9_-]+):\s*$")
FIELD = re.compile(r"^    (?P<key>[A-Za-z0-9_-]+):\s*(?P<value>.*)$")
LIST_ITEM = re.compile(r"^\s+-\s+(?P<value>\S.*?)\s*$")
MATRIX_OS = "${{ matrix.os }}"


def load_required() -> frozenset[str]:
    spec = importlib.util.spec_from_file_location("admission", ADMISSION_SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return frozenset(module.REQUIRED)


def unquote(value: str) -> str:
    value = value.split(" #", 1)[0].strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def parse_jobs(text: str) -> dict[str, dict]:
    """Read the block-style subset of ci.yml that workflow-security permits."""
    lines = text.splitlines()
    try:
        start = lines.index("jobs:")
    except ValueError:
        return {}
    jobs: dict[str, dict] = {}
    current: dict | None = None
    list_key: str | None = None
    for line in lines[start + 1 :]:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line.startswith(" "):
            break
        job = JOB_KEY.match(line)
        if job:
            current = {"name": None, "if": None, "needs": [], "os": [], "body": []}
            jobs[job.group("id")] = current
            list_key = None
            continue
        if current is None:
            continue
        current["body"].append(line.strip())
        field = FIELD.match(line)
        if field:
            key, value = field.group("key"), field.group("value")
            list_key = None
            if key in ("name", "if"):
                current[key] = unquote(value)
            elif key == "needs":
                if value:
                    current["needs"] = [unquote(value)]
                else:
                    list_key = "needs"
            continue
        stripped = line.strip()
        if stripped == "os:":
            list_key = "os"
            continue
        item = LIST_ITEM.match(line)
        if item and list_key in ("needs", "os"):
            current[list_key].append(unquote(item.group("value")))
        elif list_key == "os" and not item:
            list_key = None
    return jobs


def contexts_for(job_id: str, job: dict) -> set[str]:
    name = job["name"] or job_id
    if MATRIX_OS in name:
        return {name.replace(MATRIX_OS, os) for os in job["os"]}
    return {name}


def expected_contexts(jobs: dict[str, dict], required: frozenset[str]) -> set[str]:
    contexts: set[str] = set()
    for job_id in sorted(required | {ADMISSION_JOB}):
        if job_id in jobs:
            contexts |= contexts_for(job_id, jobs[job_id])
    return contexts


def check_ci(jobs: dict[str, dict], required: frozenset[str]) -> list[str]:
    errors = []
    for job_id in sorted(required):
        if job_id not in jobs:
            errors.append(f"ci.yml: required correctness job {job_id!r} is missing")
        elif jobs[job_id]["if"] is not None:
            errors.append(f"ci.yml: required job {job_id!r} must not be conditional")
    admission = jobs.get(ADMISSION_JOB)
    if admission is None:
        return errors + ["ci.yml: the admission job is missing"]
    if set(admission["needs"]) != required or len(admission["needs"]) != len(required):
        errors.append("ci.yml: admission needs must be exactly the REQUIRED job set")
    if admission["if"] != "always()":
        errors.append("ci.yml: admission must run with if: always()")
    if f"run: {ADMISSION_RUN}" not in admission["body"]:
        errors.append("ci.yml: admission must run check-ci-admission.py")
    if ADMISSION_NEEDS_ENV not in admission["body"]:
        errors.append("ci.yml: admission must receive toJSON(needs) as FORGE_CI_NEEDS")
    for job_id, job in jobs.items():
        if job_id != ADMISSION_JOB and ADMISSION_JOB in contexts_for(job_id, job):
            errors.append(f"ci.yml: job {job_id!r} shadows the admission context")
    return errors


def rules_by_type(ruleset: dict) -> dict[str, dict]:
    rules = ruleset.get("rules")
    if not isinstance(rules, list):
        return {}
    return {
        rule.get("type"): rule.get("parameters", {})
        for rule in rules
        if isinstance(rule, dict)
    }


def check_common(ruleset: object, label: str) -> list[str]:
    if not isinstance(ruleset, dict):
        return [f"{label}: ruleset must be a JSON object"]
    errors = []
    if ruleset.get("enforcement") != "active":
        errors.append(f"{label}: enforcement must be active")
    if ruleset.get("bypass_actors") != []:
        errors.append(f"{label}: bypass_actors must be empty; break-glass is explicit")
    return errors


def check_ruleset(ruleset: object, contexts: set[str]) -> list[str]:
    errors = check_common(ruleset, "protect-main")
    if errors and not isinstance(ruleset, dict):
        return errors
    if ruleset.get("target") != "branch":
        errors.append("protect-main: target must be branch")
    include = ruleset.get("conditions", {}).get("ref_name", {}).get("include")
    if include != ["~DEFAULT_BRANCH"]:
        errors.append("protect-main: must target exactly ~DEFAULT_BRANCH")
    rules = rules_by_type(ruleset)
    for kind in ("deletion", "non_fast_forward", "pull_request", "required_status_checks"):
        if kind not in rules:
            errors.append(f"protect-main: missing {kind} rule")
    if rules.get("pull_request", {}).get("require_code_owner_review") is not True:
        errors.append("protect-main: trust-root changes need require_code_owner_review")
    checks = rules.get("required_status_checks", {})
    if checks and checks.get("strict_required_status_checks_policy") is not True:
        errors.append("protect-main: required checks must be strict (up to date)")
    listed = checks.get("required_status_checks", []) if checks else []
    names = [item.get("context") for item in listed if isinstance(item, dict)]
    if len(names) != len(set(names)):
        errors.append("protect-main: duplicate required contexts")
    for item in listed:
        if not isinstance(item, dict) or item.get("integration_id") != GITHUB_ACTIONS_APP_ID:
            errors.append(
                f"protect-main: {item!r} must be bound to GitHub Actions "
                f"(integration_id {GITHUB_ACTIONS_APP_ID})"
            )
    if checks:
        missing = sorted(contexts - set(names))
        extra = sorted(set(names) - contexts)
        if missing:
            errors.append(f"protect-main: CI contexts not required: {missing}")
        if extra:
            errors.append(f"protect-main: required contexts CI never reports: {extra}")
    return errors


def check_tag_ruleset(ruleset: object) -> list[str]:
    errors = check_common(ruleset, "release-tags")
    if errors and not isinstance(ruleset, dict):
        return errors
    if ruleset.get("target") != "tag":
        errors.append("release-tags: target must be tag")
    include = ruleset.get("conditions", {}).get("ref_name", {}).get("include")
    if include != ["refs/tags/v*"]:
        errors.append("release-tags: must target exactly refs/tags/v*")
    rules = rules_by_type(ruleset)
    for kind in ("update", "deletion"):
        if kind not in rules:
            errors.append(f"release-tags: missing {kind} rule; tags must be immutable")
    return errors


def check_codeowners(text: str) -> list[str]:
    for line in text.splitlines():
        fields = line.split("#", 1)[0].split()
        if fields and fields[0] == TRUST_ROOT_OWNER_PATH and len(fields) > 1:
            return []
    return [f"CODEOWNERS: {TRUST_ROOT_OWNER_PATH} must have an owner"]


def validate(root: Path) -> list[str]:
    required = load_required()
    try:
        jobs = parse_jobs((root / CI_WORKFLOW).read_text(encoding="utf-8"))
        ruleset = json.loads((root / RULESET).read_text(encoding="utf-8"))
        tag_ruleset = json.loads((root / TAG_RULESET).read_text(encoding="utf-8"))
        codeowners = (root / CODEOWNERS).read_text(encoding="utf-8")
    except (OSError, UnicodeError, ValueError) as exception:
        return [f"cannot read admission contract: {exception}"]
    return (
        check_ci(jobs, required)
        + check_ruleset(ruleset, expected_contexts(jobs, required))
        + check_tag_ruleset(tag_ruleset)
        + check_codeowners(codeowners)
    )


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    root = Path(arguments[0]) if arguments else Path(".")
    errors = validate(root)
    if errors:
        print("admission contract drift:\n" + "\n".join(errors), file=sys.stderr)
        return 1
    print("admission contract: ci.yml, check-ci-admission.py, rulesets and CODEOWNERS agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
