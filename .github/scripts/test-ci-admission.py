#!/usr/bin/env python3
"""R1 regressions: aggregate checks must fail closed on incomplete evidence."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest


SCRIPT = Path(__file__).with_name("check-ci-admission.py")
SPEC = importlib.util.spec_from_file_location("admission", SCRIPT)
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)


class AdmissionTests(unittest.TestCase):
    def evidence(self):
        return {name: {"result": "success"} for name in POLICY.REQUIRED}

    def test_all_required_jobs_succeed(self):
        self.assertEqual(POLICY.validate(self.evidence()), [])

    def test_every_non_success_fails_for_each_dependency(self):
        for name in POLICY.REQUIRED:
            for result in ("failure", "cancelled", "skipped", "pending", "", None, True):
                with self.subTest(name=name, result=result):
                    needs = self.evidence()
                    needs[name]["result"] = result
                    self.assertTrue(POLICY.validate(needs))

    def test_missing_extra_or_malformed_evidence_is_denied(self):
        for name in POLICY.REQUIRED:
            needs = self.evidence()
            del needs[name]
            self.assertTrue(POLICY.validate(needs))
            for value in ({}, None, "success"):
                needs[name] = value
                self.assertTrue(POLICY.validate(needs))
        needs = self.evidence()
        needs["replacement"] = {"result": "success"}
        self.assertTrue(POLICY.validate(needs))
        for needs in ({}, [], None, "success"):
            self.assertTrue(POLICY.validate(needs))

    def test_process_exit_status(self):
        for value, expected in ((None, 1), ("invalid", 1), ("{}", 1),
                                (json.dumps(self.evidence()), 0)):
            env = dict(os.environ)
            env.pop("FORGE_CI_NEEDS", None)
            if value is not None:
                env["FORGE_CI_NEEDS"] = value
            result = subprocess.run([sys.executable, str(SCRIPT)], env=env,
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, expected, result.stderr)


if __name__ == "__main__":
    unittest.main()
