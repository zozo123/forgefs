#!/usr/bin/env python3
"""Fuzz crashes and harness failures must remain failures with retained evidence."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "fuzz_runner", Path(__file__).with_name("run-fuzz.py")
)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class FuzzEvidenceTests(unittest.TestCase):
    def test_exit_status_and_logs_survive_success_crash_and_harness_failure(self):
        for result in (0, 77, -9, OSError("cargo unavailable"),
                       subprocess.TimeoutExpired("cargo", 1200)):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as root:
                previous = Path.cwd()
                os.chdir(root)
                try:
                    def invoke(command, **kwargs):
                        kwargs["stdout"].write("retained diagnostic\n")
                        self.assertEqual(command[4], "object_decode")
                        self.assertIn("-max_total_time=600", command)
                        if isinstance(result, Exception):
                            raise result
                        return subprocess.CompletedProcess(command, result)

                    with patch.object(RUNNER.subprocess, "run", side_effect=invoke):
                        status = RUNNER.run("object_decode")
                    self.assertEqual(status, 0 if result == 0 else 1)
                    evidence = Path("fuzz/evidence")
                    outcome = json.loads((evidence / "object_decode.json").read_text())
                    self.assertEqual(outcome["target"], "object_decode")
                    self.assertGreaterEqual(outcome["elapsed_seconds"], 0)
                    self.assertEqual(outcome["returncode"],
                                     None if isinstance(result, Exception) else result)
                    self.assertEqual((evidence / "object_decode.log").read_text(),
                                     "retained diagnostic\n")
                finally:
                    os.chdir(previous)

    def test_invalid_budget_never_executes(self):
        with patch.dict(os.environ, {"FORGE_FUZZ_SECONDS": "0"}):
            with patch.object(RUNNER.subprocess, "run") as invoke:
                with self.assertRaises(ValueError):
                    RUNNER.run("object_decode")
                invoke.assert_not_called()

    def test_unknown_target_never_executes(self):
        with patch.object(RUNNER.subprocess, "run") as invoke:
            for target in ("", "../escape", "object_decode; true"):
                with self.assertRaises(ValueError):
                    RUNNER.run(target)
            invoke.assert_not_called()


if __name__ == "__main__":
    unittest.main()
