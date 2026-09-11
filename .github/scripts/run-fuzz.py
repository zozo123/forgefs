#!/usr/bin/env python3
"""Run one bounded fuzz target and preserve evidence even when it fails."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time


TARGETS = frozenset({
    "object_decode", "cap_token", "protocol_frame", "tree_name", "ref_name",
    "tar_roundtrip",
})


def run(target: str) -> int:
    if target not in TARGETS:
        raise ValueError("unknown fuzz target")
    evidence = Path("fuzz/evidence")
    evidence.mkdir(parents=True, exist_ok=True)
    command = ["cargo", "+nightly", "fuzz", "run", target, "--",
               "-max_total_time=600", "-rss_limit_mb=2048"]
    outcome = {
        "target": target,
        "commit": os.environ.get("GITHUB_SHA"),
        "run_id": os.environ.get("GITHUB_RUN_ID"),
        "command": command,
        "returncode": None,
    }
    started = time.monotonic()
    try:
        with (evidence / f"{target}.log").open("w") as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                    timeout=1200, check=False)
        outcome["returncode"] = result.returncode
        return 0 if result.returncode == 0 else 1
    except (OSError, subprocess.TimeoutExpired) as exc:
        outcome["error"] = str(exc)
        return 1
    finally:
        outcome["elapsed_seconds"] = round(time.monotonic() - started, 3)
        (evidence / f"{target}.json").write_text(
            json.dumps(outcome, indent=2) + "\n", encoding="utf-8"
        )
        print(json.dumps(outcome), flush=True)


if __name__ == "__main__":
    sys.exit(run(os.environ.get("FORGE_FUZZ_TARGET", "")))
