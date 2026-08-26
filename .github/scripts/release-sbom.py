#!/usr/bin/env python3
"""Deterministic CycloneDX SBOM normalisation and binary cross-check.

`cargo cyclonedx` emits three fields that are not a function of the source:
a random `serialNumber`, a wall-clock `metadata.timestamp`, and workspace
`bom-ref`s carrying the builder's absolute checkout path. All three are
replaced here with values derived from the release commit, so the published
SBOM is a byte-for-byte function of (commit, version, target) and can take
part in the reproducibility check like any other release asset.

`verify-binary` is the independent half: it reads the crate source paths that
rustc embeds in the shipped binary's panic locations and refuses an SBOM that
does not list a crate the binary demonstrably links.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path


# `registry/src/<index>/<name>-<version>/` as embedded by rustc in panic
# locations for dependencies resolved from a registry.
LINKED_CRATE = re.compile(
    rb"registry/src/[!-.0-~]{1,128}/"
    rb"([A-Za-z0-9_][A-Za-z0-9_.+-]{0,63})-([0-9][A-Za-z0-9_.+-]{0,63})/"
)

# Below this the cross-check would be vacuous rather than passing: a binary
# that embeds almost no crate paths cannot testify about SBOM coverage.
MINIMUM_LINKED_CRATES = 10

WORKSPACE_ROOT_REF = "path+file:///forgefs"


def deterministic_serial(version: str, commit: str, target: str) -> str:
    material = f"forgefs-sbom\0{version}\0{commit}\0{target}".encode()
    raw = bytearray(hashlib.sha256(material).digest()[:16])
    raw[6] = (raw[6] & 0x0F) | 0x50
    raw[8] = (raw[8] & 0x3F) | 0x80
    return f"urn:uuid:{uuid.UUID(bytes=bytes(raw))}"


def rewrite_paths(node: object, prefix: str) -> object:
    if isinstance(node, str):
        if node.startswith(prefix):
            return WORKSPACE_ROOT_REF + node[len(prefix) :]
        return node
    if isinstance(node, list):
        return [rewrite_paths(item, prefix) for item in node]
    if isinstance(node, dict):
        return {key: rewrite_paths(value, prefix) for key, value in node.items()}
    return node


def canonical_bytes(document: object) -> bytes:
    text = json.dumps(
        document, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )
    return (text + "\n").encode("utf-8")


def normalize(arguments: argparse.Namespace) -> int:
    document = json.loads(Path(arguments.input).read_text(encoding="utf-8"))

    root = str(Path(arguments.workspace_root).resolve())
    document = rewrite_paths(document, f"path+file://{root}")
    if not isinstance(document, dict):
        raise SystemExit("sbom root is not a JSON object")

    leaked = [
        found
        for found in re.findall(r"file://[^\"]*", canonical_bytes(document).decode())
        if root in found
    ]
    if leaked:
        raise SystemExit(f"sbom still carries builder paths: {leaked[:3]}")

    metadata = document.setdefault("metadata", {})
    metadata["timestamp"] = datetime.fromtimestamp(
        int(arguments.source_date_epoch), timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    document["serialNumber"] = deterministic_serial(
        arguments.version, arguments.commit, arguments.target
    )

    properties = [
        entry
        for entry in metadata.get("properties", [])
        if not str(entry.get("name", "")).startswith("forgefs:")
    ]
    properties.extend(
        [
            {"name": "forgefs:release:version", "value": arguments.version},
            {"name": "forgefs:release:commit", "value": arguments.commit},
            {"name": "forgefs:release:target", "value": arguments.target},
            {"name": "forgefs:release:rustc", "value": arguments.rustc},
        ]
    )
    metadata["properties"] = sorted(
        properties, key=lambda entry: (entry.get("name", ""), entry.get("value", ""))
    )

    components = document.get("components", [])
    document["components"] = sorted(
        components,
        key=lambda component: (
            component.get("name", ""),
            component.get("version", ""),
            component.get("bom-ref", ""),
        ),
    )
    dependencies = document.get("dependencies", [])
    for dependency in dependencies:
        if isinstance(dependency.get("dependsOn"), list):
            dependency["dependsOn"] = sorted(dependency["dependsOn"])
    document["dependencies"] = sorted(
        dependencies, key=lambda dependency: dependency.get("ref", "")
    )

    Path(arguments.output).write_bytes(canonical_bytes(document))
    print(
        f"sbom components={len(document['components'])} "
        f"target={arguments.target} serial={document['serialNumber']}"
    )
    return 0


def sbom_inventory(document: dict) -> set[tuple[str, str]]:
    inventory: set[tuple[str, str]] = set()
    for component in document.get("components", []):
        name = component.get("name")
        version = component.get("version")
        if isinstance(name, str) and isinstance(version, str):
            inventory.add((name, version))
    root = document.get("metadata", {}).get("component", {})
    if isinstance(root.get("name"), str) and isinstance(root.get("version"), str):
        inventory.add((root["name"], root["version"]))
    return inventory


def linked_crates(binary: Path) -> set[tuple[str, str]]:
    payload = binary.read_bytes()
    found: set[tuple[str, str]] = set()
    for name, version in LINKED_CRATE.findall(payload):
        found.add((name.decode("utf-8"), version.decode("utf-8")))
    return found


def verify_binary(arguments: argparse.Namespace) -> int:
    document = json.loads(Path(arguments.sbom).read_text(encoding="utf-8"))
    inventory = sbom_inventory(document)
    observed = linked_crates(Path(arguments.binary))

    if len(observed) < MINIMUM_LINKED_CRATES:
        print(
            "sbom cross-check is vacuous: only "
            f"{len(observed)} linked crate paths recovered from "
            f"{arguments.binary} (need {MINIMUM_LINKED_CRATES})",
            file=sys.stderr,
        )
        return 1

    missing = sorted(observed - inventory)
    if missing:
        for name, version in missing:
            print(
                f"sbom does not list linked crate {name} {version}",
                file=sys.stderr,
            )
        return 1

    print(
        f"sbom covers all {len(observed)} crates recovered from the shipped "
        f"binary ({len(inventory)} components listed)"
    )
    if arguments.report:
        lines = [f"{name} {version}" for name, version in sorted(observed)]
        Path(arguments.report).write_text(
            "linked_crates_recovered={}\nsbom_components={}\n{}\n".format(
                len(observed), len(inventory), "\n".join(lines)
            ),
            encoding="utf-8",
        )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    normalizer = subparsers.add_parser("normalize")
    normalizer.add_argument("--input", required=True)
    normalizer.add_argument("--output", required=True)
    normalizer.add_argument("--workspace-root", required=True)
    normalizer.add_argument("--version", required=True)
    normalizer.add_argument("--commit", required=True)
    normalizer.add_argument("--target", required=True)
    normalizer.add_argument("--rustc", required=True)
    normalizer.add_argument("--source-date-epoch", required=True)
    normalizer.set_defaults(handler=normalize)

    verifier = subparsers.add_parser("verify-binary")
    verifier.add_argument("--sbom", required=True)
    verifier.add_argument("--binary", required=True)
    verifier.add_argument("--report")
    verifier.set_defaults(handler=verify_binary)

    arguments = parser.parse_args()
    return int(arguments.handler(arguments))


if __name__ == "__main__":
    raise SystemExit(main())
