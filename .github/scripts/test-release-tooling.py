#!/usr/bin/env python3
"""Regression tests for the dependency-free release helpers."""

from __future__ import annotations

import hashlib
import json
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


LINKED_FIXTURE = [
    ("aho-corasick", "1.1.4"),
    ("blake3", "1.8.7"),
    ("clap_builder", "4.6.6"),
    ("hashbrown", "0.17.1"),
    ("itoa", "1.0.18"),
    ("libc", "0.2.180"),
    ("lru", "0.18.2"),
    ("rand", "0.9.5"),
    ("rusqlite", "0.40.2"),
    ("serde_json", "1.0.151"),
    ("sha2", "0.10.9"),
    ("tar", "0.4.46"),
]


def fake_binary(path: Path, crates: list[tuple[str, str]]) -> None:
    """Write a file carrying the crate paths rustc embeds in panic locations."""
    blob = bytearray(b"\x7fELF\x02\x01\x01\x00")
    for name, version in crates:
        blob += (
            f"/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/"
            f"{name}-{version}/src/lib.rs"
        ).encode()
        blob += b"\x00"
    path.write_bytes(bytes(blob))


def fake_sbom(crates: list[tuple[str, str]], root: str) -> dict:
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "serialNumber": "urn:uuid:11111111-2222-3333-4444-555555555555",
        "metadata": {
            "timestamp": "2020-01-01T00:00:00.123456789Z",
            "component": {
                "type": "application",
                "bom-ref": f"path+file://{root}/crates/forge-cli#1.2.3",
                "name": "forge",
                "version": VERSION,
            },
            "properties": [
                {"name": "cdx:rustc:sbom:target:triple", "value": "fixture-triple"}
            ],
        },
        "components": [
            {
                "type": "library",
                "bom-ref": (
                    "registry+https://github.com/rust-lang/crates.io-index#"
                    f"{name}@{version}"
                ),
                "name": name,
                "version": version,
            }
            for name, version in reversed(crates)
        ],
        "dependencies": [
            {
                "ref": f"path+file://{root}/crates/forge-cli#1.2.3",
                "dependsOn": [
                    f"path+file://{root}/crates/forge-core#1.2.3",
                    f"path+file://{root}/crates/forge-api#1.2.3",
                ],
            }
        ],
    }


def normalize_sbom(
    document: dict, directory: Path, root: str, target: str = "fixture-triple"
) -> subprocess.CompletedProcess[str]:
    source = directory / "raw.cdx.json"
    source.write_text(json.dumps(document), encoding="utf-8")
    return run(
        "release-sbom.py",
        "normalize",
        "--input",
        str(source),
        "--output",
        str(directory / "out.cdx.json"),
        "--workspace-root",
        root,
        "--version",
        VERSION,
        "--commit",
        "b" * 40,
        "--target",
        target,
        "--rustc",
        "rustc 1.97.0 (fixture)",
        "--source-date-epoch",
        "1700000000",
        cwd=directory,
    )


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
        self.assertEqual(len(names), 41)
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

            subprocess.run(
                ["git", "add", "Cargo.toml", "Cargo.lock"],
                cwd=directory,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=release-test",
                    "-c",
                    "user.email=release-test@example.invalid",
                    "commit",
                    "-qm",
                    "release",
                ],
                cwd=directory,
                check=True,
            )
            head = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=directory, text=True
            ).strip()
            subprocess.run(
                ["git", "update-ref", "refs/remotes/origin/main", head],
                cwd=directory,
                check=True,
            )
            identity_output = directory / "identity-output"
            identity = run(
                "release-identity.sh",
                cwd=directory,
                environment={
                    "REF_TYPE": "tag",
                    "REF_NAME": "v1.2.4",
                    "EVENT_NAME": "push",
                    "COMMIT_SHA": head,
                    "GITHUB_OUTPUT": str(identity_output),
                },
            )
            self.assertEqual(identity.returncode, 0, identity.stderr)
            self.assertIn("publish=true\n", identity_output.read_text(encoding="utf-8"))

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
            self.assertEqual(len(manifest.read_text(encoding="utf-8").splitlines()), 41)

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


class SbomTests(unittest.TestCase):
    def test_catalog_publishes_an_sbom_for_every_target(self) -> None:
        names = asset_names()
        sboms = [name for name in names if name.endswith(".cdx.json")]
        tarballs = [name for name in names if name.endswith(".tar.gz")]
        self.assertEqual(len(sboms), len(tarballs))
        for tarball in tarballs:
            self.assertIn(
                tarball[: -len(".tar.gz")] + ".cdx.json",
                names,
                "every published binary needs a published SBOM",
            )

    def test_catalog_publishes_reproducibility_evidence(self) -> None:
        names = asset_names()
        evidence = [name for name in names if name.endswith(".REPRODUCE.txt")]
        self.assertEqual(
            evidence, [f"forge-{VERSION}-x86_64-unknown-linux-gnu.REPRODUCE.txt"]
        )

    def test_normalized_sbom_is_a_function_of_the_release_commit(self) -> None:
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            one = Path(first)
            two = Path(second)
            for directory in (one, two):
                (directory / "crates").mkdir()
            outcome = normalize_sbom(fake_sbom(LINKED_FIXTURE, str(one)), one, str(one))
            self.assertEqual(outcome.returncode, 0, outcome.stderr)
            other = normalize_sbom(
                fake_sbom(LINKED_FIXTURE, str(two)), two, str(two)
            )
            self.assertEqual(other.returncode, 0, other.stderr)
            # Two different builder paths, one document.
            self.assertEqual(
                (one / "out.cdx.json").read_bytes(),
                (two / "out.cdx.json").read_bytes(),
            )

    def test_normalized_sbom_carries_no_builder_path_and_no_wall_clock(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            outcome = normalize_sbom(
                fake_sbom(LINKED_FIXTURE, str(directory)), directory, str(directory)
            )
            self.assertEqual(outcome.returncode, 0, outcome.stderr)
            raw = (directory / "out.cdx.json").read_text(encoding="utf-8")
            self.assertNotIn(str(directory), raw)
            document = json.loads(raw)
            self.assertEqual(
                document["metadata"]["timestamp"], "2023-11-14T22:13:20Z"
            )
            self.assertNotEqual(
                document["serialNumber"],
                "urn:uuid:11111111-2222-3333-4444-555555555555",
            )
            properties = {
                entry["name"]: entry["value"]
                for entry in document["metadata"]["properties"]
            }
            self.assertEqual(properties["forgefs:release:commit"], "b" * 40)
            self.assertEqual(properties["forgefs:release:version"], VERSION)
            self.assertEqual(properties["forgefs:release:target"], "fixture-triple")

    def test_serial_number_distinguishes_targets(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            serials = set()
            for target in ("fixture-triple", "other-triple"):
                outcome = normalize_sbom(
                    fake_sbom(LINKED_FIXTURE, str(directory)),
                    directory,
                    str(directory),
                    target=target,
                )
                self.assertEqual(outcome.returncode, 0, outcome.stderr)
                serials.add(
                    json.loads(
                        (directory / "out.cdx.json").read_text(encoding="utf-8")
                    )["serialNumber"]
                )
            self.assertEqual(len(serials), 2)

    def verify(self, directory: Path, sbom: dict, crates: list[tuple[str, str]]):
        sbom_path = directory / "sbom.json"
        sbom_path.write_text(json.dumps(sbom), encoding="utf-8")
        binary = directory / "forge"
        fake_binary(binary, crates)
        return run(
            "release-sbom.py",
            "verify-binary",
            "--sbom",
            str(sbom_path),
            "--binary",
            str(binary),
            cwd=directory,
        )

    def test_cross_check_accepts_an_sbom_that_covers_the_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            outcome = self.verify(
                directory, fake_sbom(LINKED_FIXTURE, str(directory)), LINKED_FIXTURE
            )
            self.assertEqual(outcome.returncode, 0, outcome.stderr)

    def test_cross_check_rejects_an_sbom_missing_a_linked_crate(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            sbom = fake_sbom(LINKED_FIXTURE, str(directory))
            sbom["components"] = [
                component
                for component in sbom["components"]
                if component["name"] != "blake3"
            ]
            outcome = self.verify(directory, sbom, LINKED_FIXTURE)
            self.assertNotEqual(outcome.returncode, 0)
            self.assertIn("blake3", outcome.stderr)

    def test_cross_check_rejects_a_wrong_version_of_a_linked_crate(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            sbom = fake_sbom(LINKED_FIXTURE, str(directory))
            for component in sbom["components"]:
                if component["name"] == "blake3":
                    component["version"] = "1.8.6"
            outcome = self.verify(directory, sbom, LINKED_FIXTURE)
            self.assertNotEqual(outcome.returncode, 0)

    def test_cross_check_refuses_to_pass_vacuously(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            few = LINKED_FIXTURE[:3]
            outcome = self.verify(directory, fake_sbom(few, str(directory)), few)
            self.assertNotEqual(outcome.returncode, 0)
            self.assertIn("vacuous", outcome.stderr)


if __name__ == "__main__":
    unittest.main()
