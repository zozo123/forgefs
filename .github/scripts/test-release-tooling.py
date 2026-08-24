#!/usr/bin/env python3
"""Regression tests for the dependency-free release helpers."""

from __future__ import annotations

import hashlib
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / ".github" / "scripts"
VERSION = "1.2.3"


def run(
    script: str,
    *arguments: str,
    cwd: Path,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(environment or {})
    return subprocess.run(
        [str(SCRIPTS / script), *arguments],
        cwd=cwd,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )


def asset_names(version: str = VERSION) -> list[str]:
    result = run("release-assets.sh", version, cwd=ROOT)
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    names = result.stdout.splitlines()
    if len(names) != len(set(names)):
        raise AssertionError("release asset catalog contains duplicates")
    return names


def write_payload(directory: Path, version: str = VERSION) -> list[str]:
    names = asset_names(version)
    for name in names:
        (directory / name).write_bytes(f"fixture:{name}\n".encode())
    return names


class ReleaseToolingTests(unittest.TestCase):
    def test_shell_helpers_are_regular_executables(self) -> None:
        helpers = sorted(SCRIPTS.glob("prepare-release-*.sh")) + sorted(
            SCRIPTS.glob("release-*.sh")
        )
        self.assertGreaterEqual(len(helpers), 10)
        for helper in helpers:
            with self.subTest(helper=helper.name):
                metadata = helper.lstat()
                self.assertTrue(stat.S_ISREG(metadata.st_mode))
                self.assertTrue(metadata.st_mode & 0o111)
                self.assertEqual(
                    helper.read_text(encoding="utf-8").splitlines()[0],
                    "#!/usr/bin/env bash",
                )

    def test_version_bumps_come_from_parsed_workspace_metadata(self) -> None:
        cargo_toml = """[workspace]
members = []

[workspace.package]
version = "1.2.3"
edition = "2021"
"""
        expected = {"patch": "1.2.4", "minor": "1.3.0", "major": "2.0.0"}
        for bump, next_version in expected.items():
            with self.subTest(bump=bump), tempfile.TemporaryDirectory() as temp:
                directory = Path(temp)
                (directory / "Cargo.toml").write_text(cargo_toml, encoding="utf-8")
                output = directory / "output"
                result = run(
                    "prepare-release-version.py",
                    cwd=directory,
                    environment={"BUMP": bump, "GITHUB_OUTPUT": str(output)},
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    output.read_text(encoding="utf-8"),
                    f"current=1.2.3\nnext={next_version}\n",
                )

    def test_version_update_is_bounded_to_workspace_package(self) -> None:
        cargo_toml = """[workspace]
members = []

[workspace.package]
version = "1.2.3"
edition = "2021"

[package.metadata.fixture]
version = "9.9.9"
"""
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            manifest = directory / "Cargo.toml"
            manifest.write_text(cargo_toml, encoding="utf-8")
            result = run(
                "prepare-release-update.py",
                cwd=directory,
                environment={"NEXT": "1.3.0"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            updated = manifest.read_text(encoding="utf-8")
            self.assertIn('version = "1.3.0"', updated)
            self.assertIn('version = "9.9.9"', updated)

    def test_version_update_will_not_cross_a_toml_section(self) -> None:
        cargo_toml = """[workspace]
members = []

[workspace.package]
edition = "2021"

[package.metadata.fixture]
version = "9.9.9"
"""
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            manifest = directory / "Cargo.toml"
            manifest.write_text(cargo_toml, encoding="utf-8")
            result = run(
                "prepare-release-update.py",
                cwd=directory,
                environment={"NEXT": "1.3.0"},
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(manifest.read_text(encoding="utf-8"), cargo_toml)

    def test_asset_catalog_covers_every_native_gate(self) -> None:
        names = asset_names()
        self.assertEqual(len(names), 36)
        for target in (
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ):
            self.assertIn(f"forge-{VERSION}-{target}.tar.gz", names)
            self.assertIn(f"gate-summary-{target}.json", names)

    def test_gated_patch_is_the_only_write_job_input(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "scripts").mkdir()
            (directory / "crates" / "fixture").mkdir(parents=True)
            shutil.copy2(
                ROOT / "scripts" / "verify-tag-version.sh",
                directory / "scripts" / "verify-tag-version.sh",
            )
            manifest = directory / "Cargo.toml"
            lockfile = directory / "Cargo.lock"
            manifest.write_text(
                """[workspace]
members = ["crates/fixture"]

[workspace.package]
version = "1.2.3"
edition = "2021"
""",
                encoding="utf-8",
            )
            (directory / "crates" / "fixture" / "Cargo.toml").write_text(
                """[package]
name = "fixture"
version.workspace = true
edition.workspace = true
""",
                encoding="utf-8",
            )
            lockfile.write_text(
                """# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "fixture"
version = "1.2.3"
""",
                encoding="utf-8",
            )
            subprocess.run(["git", "init", "-q"], cwd=directory, check=True)
            subprocess.run(["git", "add", "."], cwd=directory, check=True)
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=release-test",
                    "-c",
                    "user.email=release-test@example.invalid",
                    "commit",
                    "-qm",
                    "base",
                ],
                cwd=directory,
                check=True,
            )

            manifest.write_text(
                manifest.read_text(encoding="utf-8").replace("1.2.3", "1.2.4"),
                encoding="utf-8",
            )
            lockfile.write_text(
                lockfile.read_text(encoding="utf-8").replace("1.2.3", "1.2.4"),
                encoding="utf-8",
            )
            environment = {"NEXT": "1.2.4"}
            packaged = run(
                "prepare-release-package.sh",
                cwd=directory,
                environment=environment,
            )
            self.assertEqual(packaged.returncode, 0, packaged.stderr)
            self.assertEqual(
                sorted(path.name for path in (directory / "release-preparation").iterdir()),
                ["next-version", "release.patch"],
            )

            subprocess.run(
                ["git", "restore", "Cargo.toml", "Cargo.lock"],
                cwd=directory,
                check=True,
            )
            applied = run(
                "prepare-release-apply.sh",
                cwd=directory,
                environment=environment,
            )
            self.assertEqual(applied.returncode, 0, applied.stderr)
            self.assertIn('version = "1.2.4"', manifest.read_text(encoding="utf-8"))

    def test_assembly_and_verification_require_the_exact_catalog(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            payload = directory / "payload"
            payload.mkdir()
            names = write_payload(payload)
            environment = {"VERSION": VERSION}

            assembled = run(
                "release-assemble.sh", cwd=directory, environment=environment
            )
            self.assertEqual(assembled.returncode, 0, assembled.stderr)
            manifest = payload / "SHA256SUMS"
            self.assertEqual(len(manifest.read_text(encoding="utf-8").splitlines()), 36)

            verified = run(
                "release-verify-payload.sh",
                str(payload),
                cwd=ROOT,
                environment=environment,
            )
            self.assertEqual(verified.returncode, 0, verified.stderr)

            (payload / "unexpected").write_text("not published\n", encoding="utf-8")
            rejected = run(
                "release-verify-payload.sh",
                str(payload),
                cwd=ROOT,
                environment=environment,
            )
            self.assertNotEqual(rejected.returncode, 0)
            (payload / "unexpected").unlink()

            first = names[0]
            lines = manifest.read_text(encoding="utf-8").splitlines()
            manifest.write_text(
                "\n".join(line for line in lines if not line.endswith(f"  {first}"))
                + "\n",
                encoding="utf-8",
            )
            omitted = run(
                "release-verify-payload.sh",
                str(payload),
                cwd=ROOT,
                environment=environment,
            )
            self.assertNotEqual(omitted.returncode, 0)

            # Keep the imported module honest: the fixture hash format must be
            # exactly the one emitted by sha256sum.
            digest = hashlib.sha256((payload / first).read_bytes()).hexdigest()
            self.assertTrue(any(line == f"{digest}  {first}" for line in lines))


if __name__ == "__main__":
    unittest.main()
