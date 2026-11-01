#!/usr/bin/env python3
"""R1: a missing, skipped, cancelled or failed correctness job denies admission.

This aggregates evidence, not authority. The protect-main ruleset
(.github/rulesets/protect-main.json) requires this context and every job it
aggregates by name; CODEOWNERS makes changes here trust-root changes.
check-admission-contract.py keeps REQUIRED, ci.yml and the ruleset in step.
"""

import json
import os
import sys


REQUIRED = frozenset({
    "rust", "msrv", "macos-durability", "e2e-smoke", "power-loss", "gates",
})


def validate(needs: object) -> list[str]:
    if not isinstance(needs, dict) or set(needs) != REQUIRED:
        return ["CI dependency set does not match the correctness contract"]
    return [
        f"{name}: required result is not success"
        for name in sorted(REQUIRED)
        if not isinstance(needs[name], dict) or needs[name].get("result") != "success"
    ]


def main() -> int:
    try:
        needs = json.loads(os.environ["FORGE_CI_NEEDS"])
    except (KeyError, ValueError):
        print("admission denied: missing or invalid CI evidence", file=sys.stderr)
        return 1
    errors = validate(needs)
    if errors:
        print("admission denied: " + "; ".join(errors), file=sys.stderr)
        return 1
    print("admission passed: every required correctness job succeeded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
