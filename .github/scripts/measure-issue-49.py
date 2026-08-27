#!/usr/bin/env python3
"""One-shot controlled measurement for issue #49.

The script refuses to report performance unless sequential fsync calls are
visible as completed flushes on the backing block device. It then builds the
current main commit and the candidate, alternates read-heavy trials, and applies
a predeclared 10% materiality gate to median throughput or p99.
"""

from __future__ import annotations

import json
import os
import pathlib
import platform
import re
import shutil
import statistics
import subprocess
import sys
from typing import Any


BASE_SHA = "0d838e360cd8440ddacabf0c35e4ab1976a9d490"
READERS = 512
READS = 64
WORKERS = 32
REPETITIONS = 5
PROBE_FSYNC_COUNT = 64
MATERIAL_THRESHOLD_PCT = 10.0
RESULT_JSON = pathlib.Path("issue49-results.json")
RESULT_MARKDOWN = pathlib.Path("issue49-results.md")
READ_RESULT = re.compile(
    r"read fanout\s+readers=\d+\s+n=\d+\s+wall=([0-9.]+)s\s+"
    r"([0-9.]+) Hz\s*p50=([0-9.]+)ms\s+p95=([0-9.]+)ms\s+"
    r"p99=([0-9.]+)ms"
)


def output(command: list[str]) -> str:
    return subprocess.check_output(command, text=True).strip()


def run(command: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(command), flush=True)
    return subprocess.run(command, check=True, text=True, **kwargs)


def block_device_for(path: pathlib.Path) -> tuple[str, str, str, str, pathlib.Path]:
    source = output(["findmnt", "-n", "-o", "SOURCE", "-T", str(path)])
    fstype = output(["findmnt", "-n", "-o", "FSTYPE", "-T", str(path)])
    options = output(["findmnt", "-n", "-o", "OPTIONS", "-T", str(path)])
    if "nobarrier" in options.split(","):
        raise RuntimeError("measurement refused: mount is nobarrier")

    major_minor = output(["findmnt", "-n", "-o", "MAJ:MIN", "-T", str(path)])
    device_path = pathlib.Path("/sys/dev/block", major_minor).resolve(strict=True)
    device = device_path.name
    if pathlib.Path("/sys/class/block", device, "partition").is_file():
        device = device_path.parent.name
    stat_file = pathlib.Path("/sys/class/block", device, "stat")
    fields = stat_file.read_text().split()
    if len(fields) < 16:
        raise RuntimeError(
            f"measurement refused: block flush counter unavailable for {device}"
        )
    return source, fstype, options, device, stat_file


def flushes(stat_file: pathlib.Path) -> int:
    fields = stat_file.read_text().split()
    if len(fields) < 16:
        raise RuntimeError("block flush counter disappeared during probe")
    return int(fields[15])


def prove_barrier_reach(runner_temp: pathlib.Path) -> dict[str, Any]:
    source, fstype, options, device, stat_file = block_device_for(runner_temp)
    probe = runner_temp / "issue49-barrier-probe"
    shutil.rmtree(probe, ignore_errors=True)
    probe.mkdir()
    payload = b"x" * 4096
    before = flushes(stat_file)
    for index in range(PROBE_FSYNC_COUNT):
        path = probe / f"probe-{index}"
        descriptor = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        try:
            os.write(descriptor, payload)
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    directory = os.open(probe, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    after = flushes(stat_file)
    delta = after - before
    print(
        f"mount source={source} fstype={fstype} options={options} block={device}"
    )
    print(
        f"barrier probe: {PROBE_FSYNC_COUNT} sequential file fsync calls "
        f"-> {delta} completed device flushes"
    )
    if delta < PROBE_FSYNC_COUNT:
        raise RuntimeError(
            "measurement refused: barrier reach not proven "
            f"({delta} < {PROBE_FSYNC_COUNT} completed flushes)"
        )
    return {
        "source": source,
        "fstype": fstype,
        "options": options,
        "block_device": device,
        "probe_file_fsyncs": PROBE_FSYNC_COUNT,
        "completed_device_flushes": delta,
    }


def build_binaries(workspace: pathlib.Path, runner_temp: pathlib.Path) -> dict[str, pathlib.Path]:
    base_worktree = runner_temp / "forgefs-base"
    shutil.rmtree(base_worktree, ignore_errors=True)
    run(["git", "worktree", "add", "--detach", str(base_worktree), BASE_SHA])

    base_target = runner_temp / "target-base"
    head_target = runner_temp / "target-head"
    base_env = os.environ.copy()
    base_env["CARGO_TARGET_DIR"] = str(base_target)
    head_env = os.environ.copy()
    head_env["CARGO_TARGET_DIR"] = str(head_target)
    run(
        [
            "cargo",
            "build",
            "--manifest-path",
            str(base_worktree / "Cargo.toml"),
            "--release",
            "--locked",
            "-p",
            "forge-cli",
        ],
        env=base_env,
    )
    run(
        ["cargo", "build", "--release", "--locked", "-p", "forge-cli"],
        cwd=workspace,
        env=head_env,
    )
    return {
        "base": base_target / "release" / "forge",
        "head": head_target / "release" / "forge",
    }


def trial(
    binary: pathlib.Path,
    label: str,
    serial: int,
    runner_temp: pathlib.Path,
    warmup: bool,
) -> dict[str, float]:
    scratch = runner_temp / f"issue49-{label}-{serial}"
    shutil.rmtree(scratch, ignore_errors=True)
    completed = run(
        [
            str(binary),
            "bench",
            "--scratch",
            str(scratch),
            "--agents",
            "0",
            "--shared",
            "0",
            "--readers",
            str(READERS),
            "--reads",
            str(READS),
            "--workers",
            str(WORKERS),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    print(f"===== {label} {'warmup' if warmup else 'measured'} =====")
    print(completed.stdout)
    match = READ_RESULT.search(completed.stdout)
    if match is None:
        raise RuntimeError("could not parse read-fanout result")
    shutil.rmtree(scratch, ignore_errors=True)
    return {
        "wall_s": float(match.group(1)),
        "hz": float(match.group(2)),
        "p50_ms": float(match.group(3)),
        "p95_ms": float(match.group(4)),
        "p99_ms": float(match.group(5)),
    }


def render_markdown(result: dict[str, Any]) -> str:
    samples = result["samples"]
    medians = result["medians"]
    rows = []
    for label in ("base", "head"):
        median = medians[label]
        hz_samples = ", ".join(f"{sample['hz']:.1f}" for sample in samples[label])
        p99_samples = ", ".join(
            f"{sample['p99_ms']:.2f}" for sample in samples[label]
        )
        rows.append(
            f"| {label} | {median['hz']:.1f} Hz | "
            f"{median['p99_ms']:.2f} ms | {hz_samples} | {p99_samples} |"
        )
    mount = result["mount"]
    verdict = "PASS" if result["material"] else "FAIL"
    return "\n".join(
        [
            "# Issue #49: grouped observation-write measurement",
            "",
            f"Result: **{verdict}** against the predeclared "
            f"{MATERIAL_THRESHOLD_PCT:.0f}% materiality gate.",
            "",
            f"Base: {result['base_sha']}",
            f"Candidate: {result['head_sha']}",
            f"Host: {result['host']}; logical CPUs: {result['logical_cpus']}",
            f"Workload: {READERS} logical readers x {READS} reads, "
            f"{WORKERS} workers; {REPETITIONS} alternating measured "
            "repetitions after one warmup per binary.",
            "",
            "## Barrier reach",
            "",
            f"Mount: {mount['source']} ({mount['fstype']})",
            f"Options: {mount['options']}",
            f"Block device: {mount['block_device']}",
            f"Probe: {PROBE_FSYNC_COUNT} sequential file fsync calls produced "
            f"{mount['completed_device_flushes']} completed device flushes.",
            "",
            "## Results",
            "",
            "| Variant | Median throughput | Median p99 | "
            "Throughput samples | p99 samples (ms) |",
            "|---|---:|---:|---|---|",
            *rows,
            "",
            f"Throughput change: {result['throughput_gain_pct']:+.1f}%.",
            f"p99 improvement: {result['p99_improvement_pct']:+.1f}%.",
            "",
            "The patch is eligible to merge only when either controlled median "
            "throughput or median p99 improves by at least 10%. Correctness and "
            "durability gates remain separate requirements.",
            "",
        ]
    )


def measure() -> dict[str, Any]:
    workspace = pathlib.Path.cwd()
    runner_temp = pathlib.Path(os.environ["RUNNER_TEMP"])
    mount = prove_barrier_reach(runner_temp)
    binaries = build_binaries(workspace, runner_temp)
    head_sha = output(["git", "rev-parse", "HEAD"])

    serial = 0
    serial += 1
    trial(binaries["base"], "base", serial, runner_temp, warmup=True)
    serial += 1
    trial(binaries["head"], "head", serial, runner_temp, warmup=True)
    samples: dict[str, list[dict[str, float]]] = {"base": [], "head": []}
    for repetition in range(REPETITIONS):
        order = ("base", "head") if repetition % 2 == 0 else ("head", "base")
        for label in order:
            serial += 1
            samples[label].append(
                trial(binaries[label], label, serial, runner_temp, warmup=False)
            )

    medians = {
        label: {
            key: statistics.median(sample[key] for sample in rows)
            for key in rows[0]
        }
        for label, rows in samples.items()
    }
    throughput_gain_pct = (
        medians["head"]["hz"] / medians["base"]["hz"] - 1.0
    ) * 100.0
    p99_improvement_pct = (
        1.0 - medians["head"]["p99_ms"] / medians["base"]["p99_ms"]
    ) * 100.0
    material = max(throughput_gain_pct, p99_improvement_pct) >= MATERIAL_THRESHOLD_PCT
    return {
        "base_sha": BASE_SHA,
        "head_sha": head_sha,
        "host": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "readers": READERS,
        "reads_per_reader": READS,
        "workers": WORKERS,
        "repetitions": REPETITIONS,
        "mount": mount,
        "samples": samples,
        "medians": medians,
        "throughput_gain_pct": throughput_gain_pct,
        "p99_improvement_pct": p99_improvement_pct,
        "material_threshold_pct": MATERIAL_THRESHOLD_PCT,
        "material": material,
    }


def main() -> int:
    try:
        result = measure()
    except Exception as exception:
        failure = {"error": str(exception), "base_sha": BASE_SHA}
        RESULT_JSON.write_text(json.dumps(failure, indent=2) + "\n")
        RESULT_MARKDOWN.write_text(
            "# Issue #49 measurement refused\n\n" + str(exception) + "\n"
        )
        print(RESULT_MARKDOWN.read_text(), file=sys.stderr)
        return 1

    RESULT_JSON.write_text(json.dumps(result, indent=2) + "\n")
    markdown = render_markdown(result)
    RESULT_MARKDOWN.write_text(markdown)
    print(markdown)
    if not result["material"]:
        print(
            "candidate did not improve controlled median throughput or p99 by 10%",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
