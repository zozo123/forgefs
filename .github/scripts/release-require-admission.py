#!/usr/bin/env python3
"""R1 release path (#51): a tag publishes only a commit main admitted.

release.yml re-runs its own gates on the tagged commit. This adds the mainline
half of the invariant: every context the protect-main ruleset requires must
have a latest GitHub Actions check run on this exact commit that concluded
`success`. A commit that reached main without CI (a bypass, or an integration
that emitted no workflow run) cannot be released until CI runs on it and passes.

The required context list is read from .github/rulesets/protect-main.json, the
same file the repository ruleset is applied from, so it is never duplicated.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


RULESET = Path(".github/rulesets/protect-main.json")
SHA = re.compile(r"^[0-9a-f]{40}$")


def required_contexts(ruleset: dict) -> list[tuple[str, int]]:
    for rule in ruleset.get("rules", []):
        if rule.get("type") == "required_status_checks":
            return [
                (item["context"], item["integration_id"])
                for item in rule["parameters"]["required_status_checks"]
            ]
    return []


def validate(
    contexts: list[tuple[str, int]], runs: list[dict], sha: str
) -> list[str]:
    if not contexts:
        return ["ruleset names no required status checks"]
    errors = []
    for context, app_id in contexts:
        candidates = [
            run
            for run in runs
            if run.get("name") == context
            and run.get("head_sha") == sha
            and (run.get("app") or {}).get("id") == app_id
        ]
        if not candidates:
            errors.append(f"{context}: no check run from app {app_id} on {sha}")
            continue
        latest = max(candidates, key=lambda run: run.get("id", 0))
        if latest.get("status") != "completed" or latest.get("conclusion") != "success":
            errors.append(
                f"{context}: latest run is {latest.get('status')}/"
                f"{latest.get('conclusion')}, not completed/success"
            )
    return errors


def fetch_runs(repository: str, sha: str) -> list[dict]:
    output = subprocess.run(
        [
            "gh", "api", "--paginate",
            f"repos/{repository}/commits/{sha}/check-runs?per_page=100",
            "--jq", ".check_runs[]",
        ],
        check=True, capture_output=True, text=True,
    ).stdout
    return [json.loads(line) for line in output.splitlines() if line.strip()]


def main() -> int:
    repository = os.environ.get("GH_REPO", "")
    sha = os.environ.get("COMMIT_SHA", "")
    if not repository or not SHA.fullmatch(sha):
        print("::error title=Missing release identity::GH_REPO and a 40-hex COMMIT_SHA are required")
        return 1
    try:
        contexts = required_contexts(json.loads(RULESET.read_text(encoding="utf-8")))
        runs = fetch_runs(repository, sha)
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as exception:
        print(f"::error title=Admission evidence unavailable::{exception}")
        return 1
    errors = validate(contexts, runs, sha)
    if errors:
        for message in errors:
            print(f"::error title=Commit not admitted to main::{message}")
        print(
            "Tag only a main commit whose ci run passed. If main's head never ran CI, "
            "run `gh workflow run ci.yml --ref main`, wait for admission, then re-run this release.",
            file=sys.stderr,
        )
        return 1
    names = ", ".join(context for context, _ in contexts)
    print(f"mainline admission evidence: {sha} passed {names}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
