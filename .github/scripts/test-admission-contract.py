#!/usr/bin/env python3
"""R1 (#51) regressions: contract drift and unadmitted release commits fail closed."""

from __future__ import annotations

import copy
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / ".github" / "scripts"


def load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CONTRACT = load("contract", "check-admission-contract.py")
RELEASE = load("release_admission", "release-require-admission.py")
SHA = "a" * 40


class ContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tree)
        for relative in (
            CONTRACT.CI_WORKFLOW,
            CONTRACT.RULESET,
            CONTRACT.TAG_RULESET,
            CONTRACT.CODEOWNERS,
        ):
            (self.tree / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, self.tree / relative)

    def edit(self, relative: Path, old: str, new: str) -> None:
        path = self.tree / relative
        text = path.read_text(encoding="utf-8")
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1), encoding="utf-8")

    def ruleset(self, relative: Path = CONTRACT.RULESET) -> dict:
        return json.loads((self.tree / relative).read_text(encoding="utf-8"))

    def write_ruleset(self, document: dict, relative: Path = CONTRACT.RULESET) -> None:
        (self.tree / relative).write_text(json.dumps(document), encoding="utf-8")

    def assertDrift(self, fragment: str) -> None:
        errors = CONTRACT.validate(self.tree)
        self.assertTrue(any(fragment in error for error in errors), errors)

    def test_repository_contract_is_consistent(self) -> None:
        self.assertEqual(CONTRACT.validate(ROOT), [])
        self.assertEqual(CONTRACT.validate(self.tree), [])

    def test_matrix_names_expand_to_every_runner(self) -> None:
        jobs = CONTRACT.parse_jobs((ROOT / CONTRACT.CI_WORKFLOW).read_text(encoding="utf-8"))
        self.assertEqual(
            CONTRACT.contexts_for("macos-durability", jobs["macos-durability"]),
            {"macOS durability (macos-15)", "macOS durability (macos-15-intel)"},
        )

    def test_renamed_gate_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "  gates:\n", "  gates-lite:\n")
        self.assertDrift("'gates' is missing")

    def test_renamed_context_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "name: power-loss durability (I4)", "name: power-loss")
        self.assertDrift("CI contexts not required")
        self.assertDrift("required contexts CI never reports")

    def test_dropped_matrix_runner_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "          - macos-15-intel\n", "")
        self.assertDrift("required contexts CI never reports")

    def test_weakened_admission_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "      - power-loss\n      - gates\n    runs-on", "      - gates\n    runs-on")
        self.assertDrift("admission needs")

    def test_conditional_admission_or_gate_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "    name: admission\n    if: always()\n", "    name: admission\n    if: success()\n")
        self.assertDrift("if: always()")
        self.edit(CONTRACT.CI_WORKFLOW, "  msrv:\n", "  msrv:\n    if: false\n")
        self.assertDrift("'msrv' must not be conditional")

    def test_skipped_or_soft_failing_gate_step_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "      - name: Clippy\n", "      - name: Clippy\n        if: false\n")
        self.assertDrift("'rust' has a conditional step")
        self.edit(CONTRACT.CI_WORKFLOW, "      - name: CLI ABI conformance\n", "      - name: CLI ABI conformance\n        continue-on-error: true\n")
        self.assertDrift("'gates' uses continue-on-error")

    def test_evidence_upload_after_failure_is_allowed(self) -> None:
        jobs = CONTRACT.parse_jobs((ROOT / CONTRACT.CI_WORKFLOW).read_text(encoding="utf-8"))
        self.assertIn("if: always()", jobs["gates"]["body"])
        self.assertEqual(CONTRACT.check_required_steps("gates", jobs["gates"]), [])

    def test_replaced_admission_command_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "run: python3 .github/scripts/check-ci-admission.py", "run: python3 .github/scripts/test-ci-admission.py")
        self.assertDrift("must run check-ci-admission.py")

    def test_shadow_admission_job_is_drift(self) -> None:
        self.edit(CONTRACT.CI_WORKFLOW, "  rust:\n", "  shadow:\n    name: admission\n    runs-on: ubuntu-24.04\n  rust:\n")
        self.assertDrift("shadows the admission context")

    def test_ruleset_weakening_is_drift(self) -> None:
        cases = {
            "enforcement must be active": lambda d: d.update(enforcement="evaluate"),
            "bypass_actors must be empty": lambda d: d.update(bypass_actors=[{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}]),
            "missing pull_request rule": lambda d: d.update(rules=[r for r in d["rules"] if r["type"] != "pull_request"]),
            "missing deletion rule": lambda d: d.update(rules=[r for r in d["rules"] if r["type"] != "deletion"]),
            "missing non_fast_forward rule": lambda d: d.update(rules=[r for r in d["rules"] if r["type"] != "non_fast_forward"]),
            "missing required_status_checks rule": lambda d: d.update(rules=[r for r in d["rules"] if r["type"] != "required_status_checks"]),
            "~DEFAULT_BRANCH": lambda d: d["conditions"]["ref_name"].update(include=[]),
            "excluding nothing": lambda d: d["conditions"]["ref_name"].update(exclude=["refs/heads/main"]),
        }
        original = self.ruleset()
        for fragment, mutate in cases.items():
            with self.subTest(fragment=fragment):
                document = copy.deepcopy(original)
                mutate(document)
                self.write_ruleset(document)
                self.assertDrift(fragment)

    def test_required_check_details_are_drift(self) -> None:
        original = self.ruleset()
        def checks(document: dict) -> dict:
            return next(r for r in document["rules"] if r["type"] == "required_status_checks")["parameters"]
        cases = {
            "bound to GitHub Actions": lambda d: checks(d)["required_status_checks"][0].pop("integration_id"),
            "CI contexts not required": lambda d: checks(d)["required_status_checks"].pop(),
            "duplicate required contexts": lambda d: checks(d)["required_status_checks"].append(dict(checks(d)["required_status_checks"][0])),
            "strict": lambda d: checks(d).update(strict_required_status_checks_policy=False),
            "require_code_owner_review": lambda d: next(r for r in d["rules"] if r["type"] == "pull_request")["parameters"].update(require_code_owner_review=False),
        }
        for fragment, mutate in cases.items():
            with self.subTest(fragment=fragment):
                document = copy.deepcopy(original)
                mutate(document)
                self.write_ruleset(document)
                self.assertDrift(fragment)

    def test_mutable_release_tags_are_drift(self) -> None:
        document = self.ruleset(CONTRACT.TAG_RULESET)
        document["rules"] = [r for r in document["rules"] if r["type"] != "update"]
        self.write_ruleset(document, CONTRACT.TAG_RULESET)
        self.assertDrift("tags must be immutable")
        document["conditions"]["ref_name"]["exclude"] = ["refs/tags/v1*"]
        self.write_ruleset(document, CONTRACT.TAG_RULESET)
        self.assertDrift("release-tags: must target exactly refs/tags/v*, excluding nothing")

    def test_unowned_trust_root_is_drift(self) -> None:
        for text in (
            "# nobody\n",
            "/.github/\n",
            "/.github/ @zozo123\n/.github/\n",
            "/.github/ @zozo123\n/.github/workflows/ @someone-else\n",
        ):
            with self.subTest(text=text):
                (self.tree / CONTRACT.CODEOWNERS).write_text(text, encoding="utf-8")
                self.assertDrift("must be the last rule")

    def test_unreadable_contract_fails_closed(self) -> None:
        (self.tree / CONTRACT.RULESET).write_text("{", encoding="utf-8")
        self.assertDrift("cannot read admission contract")


def run(name: str, conclusion: str = "success", ident: int = 1, sha: str = SHA,
        app: int = 15368, status: str = "completed") -> dict:
    return {"id": ident, "name": name, "head_sha": sha, "status": status,
            "conclusion": conclusion, "app": {"id": app}}


class ReleaseAdmissionTests(unittest.TestCase):
    def setUp(self) -> None:
        ruleset = json.loads((ROOT / RELEASE.RULESET).read_text(encoding="utf-8"))
        self.contexts = RELEASE.required_contexts(ruleset)
        self.green = [run(name, ident=index) for index, (name, _) in enumerate(self.contexts, 1)]

    def test_contexts_come_from_the_ruleset(self) -> None:
        names = [name for name, _ in self.contexts]
        self.assertIn("admission", names)
        self.assertIn("gates", names)
        self.assertEqual(RELEASE.required_contexts({"rules": []}), [])
        self.assertTrue(RELEASE.validate([], self.green, SHA))

    def test_all_green_is_admitted(self) -> None:
        self.assertEqual(RELEASE.validate(self.contexts, self.green, SHA), [])

    def test_missing_failed_or_foreign_evidence_is_denied(self) -> None:
        for index, (name, _) in enumerate(self.contexts):
            for replacement in (
                None,
                run(name, conclusion="failure"),
                run(name, conclusion="skipped"),
                run(name, conclusion=None, status="in_progress"),
                run(name, sha="b" * 40),
                run(name, app=1),
            ):
                with self.subTest(name=name, replacement=replacement):
                    runs = [r for r in self.green if r["name"] != name]
                    if replacement is not None:
                        runs.append(replacement)
                    self.assertTrue(RELEASE.validate(self.contexts, runs, SHA))

    def test_latest_rerun_decides(self) -> None:
        name = self.contexts[0][0]
        failed_after_success = self.green + [run(name, conclusion="failure", ident=999)]
        self.assertTrue(RELEASE.validate(self.contexts, failed_after_success, SHA))
        success_after_failure = [r for r in self.green if r["name"] != name] + [
            run(name, conclusion="failure", ident=1000),
            run(name, ident=1001),
        ]
        self.assertEqual(RELEASE.validate(self.contexts, success_after_failure, SHA), [])

    def test_process_queries_check_runs_and_exits_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            fake = directory / "gh"
            for runs, expected in ((self.green, 0), (self.green[1:], 1)):
                lines = "\n".join(json.dumps(r) for r in runs)
                fake.write_text(
                    "#!/usr/bin/env bash\n"
                    f"test \"$3\" = 'repos/o/r/commits/{SHA}/check-runs?per_page=100' || exit 9\n"
                    f"cat <<'JSON'\n{lines}\nJSON\n",
                    encoding="utf-8",
                )
                fake.chmod(0o755)
                env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}",
                           GH_REPO="o/r", COMMIT_SHA=SHA)
                result = subprocess.run(
                    [sys.executable, str(SCRIPTS / "release-require-admission.py")],
                    cwd=ROOT, env=env, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
            env["COMMIT_SHA"] = "not-a-sha"
            result = subprocess.run(
                [sys.executable, str(SCRIPTS / "release-require-admission.py")],
                cwd=ROOT, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main()
