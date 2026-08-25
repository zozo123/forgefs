#!/usr/bin/env python3
"""Git-worktree side of the docs/BENCH.md W7 comparator, plus the shared
durability-equivalence verdict rule.

W7 compares ForgeFS W1 (N agents, one small edit, one checkin, private refs)
against the Git equivalent: N agents, each in its own worktree on its own
branch, one small edit, one commit.

Three things live here so that they can be tested rather than asserted:

* the Git workload, with the same per-agent operation shape as W1;
* the barrier census for one Git agent operation, via the optional
  scripts/w7_fsync_probe.c LD_PRELOAD shim;
* classify_equivalence(), the rule that decides whether a ForgeFS/Git speed
  ratio may be published at all.

Run scripts/w7-git-comparator.sh instead of calling this directly; that script
runs both sides, records the required environment line, and applies the
verdict. Run "w7_git_worktree_bench.py selftest" to check the pure rules.

Stdlib only, and deliberately no ForgeFS import: the comparator must not be
able to flatter ForgeFS by sharing its timing code.
"""

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
import threading
import time

# Durability configurations. "default" sets nothing at all: it measures the
# Git this machine actually ships, which is the number a user gets by typing
# "git commit". "durable" asks Git for every barrier it knows how to make.
DURABILITY = {
    "default": [],
    "durable": [("core.fsync", "all"), ("core.fsyncMethod", "fsync")],
}

# Config that is about determinism and noise, never about durability. These
# are applied to both modes so the two Git numbers differ only in durability.
NEUTRAL_CONFIG = [
    ("user.name", "w7 bench"),
    ("user.email", "w7@bench.invalid"),
    ("commit.gpgsign", "false"),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
    ("advice.detachedHead", "false"),
]


def rust_round(x):
    """Match Rust f64::round (half away from zero) for non-negative x, so the
    Git percentiles use the same index rule as forge-api bench.rs."""
    return int(math.floor(x + 0.5))


def percentiles(samples_us):
    """Same shape and index rule as Percentiles::from_us in
    crates/forge-api/src/bench.rs, so the two sides are read the same way."""
    s = sorted(samples_us)
    n = len(s)
    if n == 0:
        return {"n": 0, "p50_us": 0, "p95_us": 0, "p99_us": 0, "max_us": 0}
    return {
        "n": n,
        "p50_us": s[rust_round((n - 1) * 0.50)],
        "p95_us": s[rust_round((n - 1) * 0.95)],
        "p99_us": s[rust_round((n - 1) * 0.99)],
        "max_us": s[-1],
    }


def classify_equivalence(forge, git):
    """The docs/BENCH.md W7 gate, as code.

    forge and git are barrier censuses for ONE logical agent operation:
        {"available": bool, "file": int, "dir": int}

    Returns (verdict, reason). verdict is "comparable" only when both paths
    were actually observed to issue the same classes of durability barrier.
    Anything else is non-comparable, and a non-comparable verdict forbids
    publishing the speed ratio as a ratio. Same durability *intent* is not
    enough; unobserved is not equivalent.
    """
    if not forge.get("available") or not git.get("available"):
        return (
            "non-comparable",
            "durability unknown: the barrier probe did not run on both sides, "
            "so equivalence was not demonstrated",
        )
    for kind in ("file", "dir"):
        f = int(forge.get(kind, 0))
        g = int(git.get(kind, 0))
        if f > 0 and g == 0:
            return (
                "non-comparable",
                "durability mismatch: ForgeFS issues {} {} barrier(s) for the "
                "measured operation and Git issues 0, so Git is not persisting "
                "what ForgeFS persists".format(f, kind),
            )
        if g > 0 and f == 0:
            return (
                "non-comparable",
                "durability mismatch: Git issues {} {} barrier(s) for the "
                "measured operation and ForgeFS issues 0".format(g, kind),
            )
    return (
        "comparable",
        "both paths were observed to issue file and directory durability "
        "barriers for the measured operation (ForgeFS file={}/dir={}, "
        "Git file={}/dir={})".format(
            forge.get("file", 0),
            forge.get("dir", 0),
            git.get("file", 0),
            git.get("dir", 0),
        ),
    )


def git_cmd(args, cwd, env=None, check=True):
    proc = subprocess.run(
        ["git"] + args,
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            "git {} failed in {}: rc={} stderr={}".format(
                " ".join(args), cwd, proc.returncode, proc.stderr.decode("utf-8", "replace")
            )
        )
    return proc


def config_repo(repo, mode):
    for key, value in NEUTRAL_CONFIG + DURABILITY[mode]:
        git_cmd(["config", key, value], cwd=repo)


def init_repo(repo, mode):
    os.makedirs(repo, exist_ok=True)
    git_cmd(["init", "-q", "-b", "main", "."], cwd=repo)
    config_repo(repo, mode)
    with open(os.path.join(repo, "README"), "w", encoding="utf-8") as fh:
        fh.write("w7 comparator base\n")
    git_cmd(["add", "--", "README"], cwd=repo)
    git_cmd(["commit", "-q", "-m", "base"], cwd=repo)


def add_worktrees(repo, wt_root, agents):
    """One worktree and one branch per agent: the Git analogue of one
    capability-scoped session per agent on its own heads/agents/* ref."""
    os.makedirs(wt_root, exist_ok=True)
    paths = []
    t0 = time.perf_counter()
    for i in range(agents):
        path = os.path.join(wt_root, "a{}".format(i))
        git_cmd(
            ["worktree", "add", "-q", "-b", "agents/bench{}".format(i), path, "main"],
            cwd=repo,
        )
        paths.append(path)
    return paths, time.perf_counter() - t0


def agent_commit(i, wt, env=None):
    """One logical W1 operation, Git side: write /w{i}.txt containing
    "agent {i}", stage it, commit it. Object writes, index update and ref
    update are all inside the timed region, exactly as the ForgeFS checkin
    writes objects and publishes a ref inside its own."""
    name = "w{}.txt".format(i)
    t0 = time.perf_counter_ns()
    with open(os.path.join(wt, name), "w", encoding="utf-8") as fh:
        fh.write("agent {}".format(i))
    git_cmd(["add", "--", name], cwd=wt, env=env)
    git_cmd(["commit", "-q", "-m", "bench"], cwd=wt, env=env)
    return (time.perf_counter_ns() - t0) // 1000


def agent_noop(i, wt, env=None):
    """Process-spawn floor: the cheapest useful git command, driven by the
    same pool. ForgeFS W1 runs in-process threads and Git runs two execs per
    agent, so this number is what a reader must subtract before attributing
    any part of the gap to storage design."""
    t0 = time.perf_counter_ns()
    git_cmd(["rev-parse", "--verify", "HEAD"], cwd=wt, env=env)
    return (time.perf_counter_ns() - t0) // 1000


def drive(op, worktrees, workers):
    """Bounded worker pool over N agent tasks, mirroring the bounded-worker
    runner in crates/forge-api/src/soak.rs."""
    n = len(worktrees)
    workers = max(1, min(workers, n))
    results = [0] * n
    errors = []
    counter = {"next": 0}
    lock = threading.Lock()

    def worker():
        while True:
            with lock:
                i = counter["next"]
                counter["next"] += 1
            if i >= n:
                return
            try:
                results[i] = op(i, worktrees[i])
            except Exception as exc:  # noqa: BLE001 - reported, never swallowed
                with lock:
                    errors.append("agent {}: {}".format(i, exc))
                return

    threads = [threading.Thread(target=worker) for _ in range(workers)]
    t0 = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t0
    if errors:
        raise RuntimeError("; ".join(errors[:4]))
    return results, wall


def verify(repo, agents):
    """Correctness gate. Every agent must have landed exactly one commit on
    its own branch with the exact bytes, and the object store must be sound.
    A benchmark that lost writes is not a faster benchmark."""
    for i in range(agents):
        ref = "agents/bench{}".format(i)
        count = git_cmd(["rev-list", "--count", ref], cwd=repo).stdout.decode().strip()
        if count != "2":
            raise RuntimeError("{} has {} commits, expected 2".format(ref, count))
        blob = git_cmd(
            ["show", "{}:w{}.txt".format(ref, i)], cwd=repo
        ).stdout.decode("utf-8")
        if blob != "agent {}".format(i):
            raise RuntimeError("{} content mismatch: {!r}".format(ref, blob))
    git_cmd(["fsck", "--strict", "--no-progress"], cwd=repo)
    return "ok"


def census(log_path):
    counts = {"file": 0, "dir": 0, "unknown": 0}
    with open(log_path, "r", encoding="utf-8") as fh:
        for line in fh:
            parts = line.split()
            if len(parts) == 2 and parts[1] in counts:
                counts[parts[1]] += 1
    counts["total"] = counts["file"] + counts["dir"] + counts["unknown"]
    counts["available"] = True
    return counts


def probe_barriers(root, mode, probe_lib):
    """Barrier census for ONE Git agent operation under this durability mode.

    Deliberately a fresh repository outside the timed region: it measures what
    the operation persists, not how fast it does it."""
    if not probe_lib or not os.path.exists(probe_lib):
        return {"available": False, "reason": "probe library not built"}
    probe_root = os.path.join(root, "probe-{}".format(mode))
    shutil.rmtree(probe_root, ignore_errors=True)
    repo = os.path.join(probe_root, "repo")
    init_repo(repo, mode)
    paths, _ = add_worktrees(repo, os.path.join(probe_root, "wt"), 1)
    log_path = os.path.join(probe_root, "barriers.log")
    env = dict(os.environ)
    env["LD_PRELOAD"] = os.path.abspath(probe_lib)
    env["W7_FSYNC_LOG"] = log_path
    open(log_path, "w").close()
    agent_commit(0, paths[0], env=env)
    counts = census(log_path)
    counts["scope"] = "git add + git commit for one agent, mode={}".format(mode)
    return counts


def observed_config(repo):
    proc = git_cmd(["config", "--show-origin", "--list"], cwd=repo, check=False)
    lines = proc.stdout.decode("utf-8", "replace").splitlines()
    keys = ("core.fsync", "core.fsyncmethod", "core.fsyncobjectfiles", "receive.fsync")
    return [ln for ln in lines if any(k in ln.lower() for k in keys)]


def cmd_run(args):
    root = os.path.abspath(args.root)
    if os.path.exists(root):
        raise RuntimeError("root path already exists: {}".format(root))
    os.makedirs(root)
    repo = os.path.join(root, "repo")
    init_repo(repo, args.durability)
    worktrees, setup_wall = add_worktrees(repo, os.path.join(root, "wt"), args.agents)

    latencies, wall = drive(agent_commit, worktrees, args.workers)
    floor, floor_wall = drive(agent_noop, worktrees, args.workers)
    fsck = verify(repo, args.agents)

    out = {
        "side": "git-worktree",
        "workload": "W7 (ForgeFS W1 logical shape)",
        "durability_mode": args.durability,
        "durability_config_applied": [list(kv) for kv in DURABILITY[args.durability]],
        "durability_config_observed": observed_config(repo),
        "git_version": subprocess.run(
            ["git", "--version"], stdout=subprocess.PIPE
        ).stdout.decode().strip(),
        "agents": args.agents,
        "workers": args.workers,
        "wall_s": wall,
        "ops_s": args.agents / wall if wall > 0 else 0.0,
        "latency": percentiles(latencies),
        "worktree_setup_wall_s": setup_wall,
        "exec_floor": {
            "wall_s": floor_wall,
            "ops_s": args.agents / floor_wall if floor_wall > 0 else 0.0,
            "latency": percentiles(floor),
        },
        "execs_per_agent_op": 2,
        "fsck": fsck,
        "barriers_per_agent_op": probe_barriers(root, args.durability, args.probe_lib),
    }
    text = json.dumps(out, indent=2, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            fh.write(text + "\n")
    print(text)
    if not args.keep:
        git_cmd(["worktree", "prune"], cwd=repo, check=False)
        shutil.rmtree(root, ignore_errors=True)
    return 0


def cmd_verdict(args):
    with open(args.forge, "r", encoding="utf-8") as fh:
        forge = json.load(fh)
    with open(args.git, "r", encoding="utf-8") as fh:
        git = json.load(fh)
    if "barriers_per_agent_op" in git:
        git = git["barriers_per_agent_op"]
    verdict, reason = classify_equivalence(forge, git)
    print(json.dumps({"verdict": verdict, "reason": reason}, indent=2, sort_keys=True))
    return 0


def cmd_selftest(_args):
    failures = []

    def check(name, got, want):
        if got != want:
            failures.append("{}: got {!r} want {!r}".format(name, got, want))

    p = percentiles([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
    check("percentile n", p["n"], 10)
    check("percentile p50", p["p50_us"], 6)
    check("percentile p95", p["p95_us"], 10)
    check("percentile max", p["max_us"], 10)
    check("percentile empty", percentiles([])["p50_us"], 0)

    forge = {"available": True, "file": 5, "dir": 4}
    # Git at its default: no barriers at all for the measured operation.
    check(
        "git default must be non-comparable",
        classify_equivalence(forge, {"available": True, "file": 0, "dir": 0})[0],
        "non-comparable",
    )
    # The dangerous case: Git fsyncs object files but never their directories,
    # so the ratio still may not be published as a ratio.
    check(
        "git file-only barriers must be non-comparable",
        classify_equivalence(forge, {"available": True, "file": 3, "dir": 0})[0],
        "non-comparable",
    )
    check(
        "unprobed sides must be non-comparable",
        classify_equivalence(forge, {"available": False})[0],
        "non-comparable",
    )
    check(
        "observed file+dir barriers on both sides are comparable",
        classify_equivalence(forge, {"available": True, "file": 3, "dir": 2})[0],
        "comparable",
    )
    check(
        "a ForgeFS regression to zero barriers is also non-comparable",
        classify_equivalence(
            {"available": True, "file": 0, "dir": 0}, {"available": True, "file": 3, "dir": 2}
        )[0],
        "non-comparable",
    )
    if failures:
        for f in failures:
            print("w7 selftest FAIL: {}".format(f), file=sys.stderr)
        return 1
    print("w7 selftest: PASS")
    return 0



FORGE_PRIVATE_RE = re.compile(
    r"private checkin\s+n=(?P<ok>\d+)/(?P<n>\d+)\s+wall=(?P<wall>[0-9.]+)s\s+(?P<hz>[0-9.]+) Hz"
)
FORGE_PCTL_RE = re.compile(
    r"p50=(?P<p50>[0-9.]+)ms\s+p95=(?P<p95>[0-9.]+)ms\s+p99=(?P<p99>[0-9.]+)ms\s+max=(?P<max>[0-9.]+)ms"
)


def parse_forge_bench(text):
    """Read the checked-in `forge bench` report.

    W7 is a W1-only comparison, so only the private-checkin block is read.
    The serial baseline and the lifetime counter block are carried through
    verbatim, never divided by a checkin count: docs/BENCH.md rules the
    lifetime totals out as per-operation costs."""
    m = FORGE_PRIVATE_RE.search(text)
    if not m:
        raise RuntimeError("no private-checkin line in forge bench output")
    tail = text[m.end():]
    q = FORGE_PCTL_RE.search(tail)
    if not q:
        raise RuntimeError("no private percentile line in forge bench output")
    ok = int(m.group("ok"))
    n = int(m.group("n"))
    if ok != n:
        raise RuntimeError("forge bench lost writers: {}/{}".format(ok, n))

    def line(prefix):
        for ln in text.splitlines():
            if ln.startswith(prefix):
                return ln.strip()
        return "unavailable"

    return {
        "side": "forgefs",
        "workload": "W1 private (the W7 comparison workload)",
        "durability_mode": "synchronous=FULL, object file+dir fsync",
        "agents": n,
        "updated": ok,
        "wall_s": float(m.group("wall")),
        "ops_s": float(m.group("hz")),
        "latency": {
            "n": n,
            "p50_us": int(round(float(q.group("p50")) * 1000)),
            "p95_us": int(round(float(q.group("p95")) * 1000)),
            "p99_us": int(round(float(q.group("p99")) * 1000)),
            "max_us": int(round(float(q.group("max")) * 1000)),
        },
        "durability_line": line("durability"),
        "storage_lifetime": line("storage lifetime"),
        "sqlite_lifetime": line("sqlite lifetime"),
        "fsck": "ok (forge bench fails the run otherwise)",
    }


def cmd_parse_forge(args):
    with open(args.input, "r", encoding="utf-8") as fh:
        out = parse_forge_bench(fh.read())
    out["workers"] = args.workers
    text = json.dumps(out, indent=2, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            fh.write(text + "\n")
    else:
        print(text)
    return 0


def median_low(values):
    """docs/BENCH.md summarises run-level medians. median_low reports a value
    that was actually observed in some repetition instead of averaging two
    runs into a number nobody measured."""
    s = sorted(values)
    return s[(len(s) - 1) // 2]


def _load_runs(out_dir, prefix):
    names = sorted(
        n for n in os.listdir(out_dir) if n.startswith(prefix) and n.endswith(".json")
    )
    runs = []
    for n in names:
        with open(os.path.join(out_dir, n), "r", encoding="utf-8") as fh:
            runs.append(json.load(fh))
    return runs


def _summary(runs):
    if not runs:
        return None
    return {
        "reps": len(runs),
        "ops_s": median_low([r["ops_s"] for r in runs]),
        "p50_ms": median_low([r["latency"]["p50_us"] for r in runs]) / 1000.0,
        "p95_ms": median_low([r["latency"]["p95_us"] for r in runs]) / 1000.0,
        "p99_ms": median_low([r["latency"]["p99_us"] for r in runs]) / 1000.0,
        "max_ms": median_low([r["latency"]["max_us"] for r in runs]) / 1000.0,
        "all_ops_s": [r["ops_s"] for r in runs],
    }


def cmd_report(args):
    out_dir = args.out_dir
    forge_runs = _load_runs(out_dir, "forge-rep")
    forge_cli_runs = _load_runs(out_dir, "forgecli-rep")
    git_default = _load_runs(out_dir, "git-default-rep")
    git_durable = _load_runs(out_dir, "git-durable-rep")
    if not forge_runs or not git_default or not git_durable:
        raise RuntimeError("missing repetition files under {}".format(out_dir))
    with open(os.path.join(out_dir, "forge-barriers.json"), "r", encoding="utf-8") as fh:
        forge_barriers = json.load(fh)

    verdicts = {}
    for mode, runs in (("default", git_default), ("durable", git_durable)):
        verdicts[mode] = classify_equivalence(
            forge_barriers, runs[0].get("barriers_per_agent_op", {"available": False})
        )

    f = _summary(forge_runs)
    fcli = _summary(forge_cli_runs)
    g = {"default": _summary(git_default), "durable": _summary(git_durable)}
    env_line = "unavailable"
    env_path = os.path.join(out_dir, "env-line.txt")
    if os.path.exists(env_path):
        with open(env_path, "r", encoding="utf-8") as fh:
            env_line = fh.read().strip()

    L = []
    add = L.append
    add("### W7 -- ForgeFS vs Git worktrees")
    add("")
    add("Workload W7 (W1 logical shape): {} agents, {} workers, one small edit and".format(
        forge_runs[0]["agents"], args.workers))
    add("one commit per agent onto its own ref, {} fresh-repository repetitions per".format(len(forge_runs)))
    add("configuration. Medians are run-level medians (median_low).")
    add("")
    add("| Configuration | ops/s | p50 ms | p95 ms | p99 ms | max ms |")
    add("|---|---|---|---|---|---|")
    add("| ForgeFS (synchronous=FULL, file+dir fsync) | {:.1f} | {:.2f} | {:.2f} | {:.2f} | {:.2f} |".format(
        f["ops_s"], f["p50_ms"], f["p95_ms"], f["p99_ms"], f["max_ms"]))
    if fcli:
        add("| ForgeFS through the `forge` CLI, same durability (3 execs/agent) "
            "| {:.1f} | {:.2f} | {:.2f} | {:.2f} | {:.2f} |".format(
                fcli["ops_s"], fcli["p50_ms"], fcli["p95_ms"], fcli["p99_ms"],
                fcli["max_ms"]))
    for mode in ("default", "durable"):
        s = g[mode]
        label = "Git worktrees, {}".format(
            "as-shipped default" if mode == "default" else "core.fsync=all fsyncMethod=fsync")
        add("| {} | {:.1f} | {:.2f} | {:.2f} | {:.2f} | {:.2f} |".format(
            label, s["ops_s"], s["p50_ms"], s["p95_ms"], s["p99_ms"], s["max_ms"]))
    add("")
    add("Observed durability barriers for one agent operation:")
    add("")
    add("| Path | file fsync | dir fsync |")
    add("|---|---|---|")
    add("| ForgeFS write+checkin | {} | {} |".format(
        forge_barriers.get("file", "unavailable"), forge_barriers.get("dir", "unavailable")))
    for mode, runs in (("default", git_default), ("durable", git_durable)):
        b = runs[0].get("barriers_per_agent_op", {})
        add("| Git add+commit, {} | {} | {} |".format(
            mode, b.get("file", "unavailable"), b.get("dir", "unavailable")))
    add("")
    add("Durability-equivalence gate (docs/BENCH.md W7):")
    add("")
    for mode in ("default", "durable"):
        verdict, reason = verdicts[mode]
        if verdict == "comparable":
            ratio = f["ops_s"] / g[mode]["ops_s"] if g[mode]["ops_s"] else 0.0
            add("- Git {}: **comparable** -- ratio ForgeFS/Git = {:.2f}x. {}".format(
                mode, ratio, reason))
        else:
            add("- Git {}: **non-comparable: durability mismatch/unknown** -- {}. "
                "The two ops/s numbers above stand on their own; their quotient is "
                "not a speed ratio.".format(mode, reason))
    add("")
    add("Process-spawn floor (same driver, `git rev-parse --verify HEAD` per agent): "
        "{:.1f} ops/s, p50 {:.2f} ms. Each Git agent operation costs two more execs "
        "than that; ForgeFS W1 runs in-process threads. Subtract this before "
        "attributing any part of the gap to storage design.".format(
            git_default[0]["exec_floor"]["ops_s"],
            git_default[0]["exec_floor"]["latency"]["p50_us"] / 1000.0))
    floor_ops = git_default[0]["exec_floor"]["ops_s"]
    add("")
    add("Process-model note. `forge bench` drives its agents as in-process threads "
        "while Git execs twice per agent. The `forge` CLI row above runs the same "
        "ForgeFS work with three execs per agent and is the row to compare against "
        "Git if your agents shell out. At {} workers the no-op Git exec floor alone "
        "is {:.1f} ops/s, so any Git deficit smaller than that floor is "
        "process-model cost and not storage design.".format(args.workers, floor_ops))
    add("")
    add("Excluded from every timed region on both sides: repository creation, and per "
        "agent worktree/branch creation (Git, median {:.3f} s for all agents) versus "
        "capability grant and session open (ForgeFS, inside the W1 timed region).".format(
            median_low([r["worktree_setup_wall_s"] for r in git_default])))
    add("")
    add("Correctness gate: ForgeFS `fsck --full` inside `forge bench`; Git `fsck "
        "--strict` plus a per-agent check that every branch carries exactly one new "
        "commit with the exact bytes. All runs passed.")
    add("")
    add("Environment line:")
    add("")
    add("```text")
    add(env_line)
    add("```")
    add("")
    add("ForgeFS lifetime counters, repetition 1 (whole-process totals; never divide "
        "by the checkin count):")
    add("")
    add("```text")
    add(forge_runs[0]["durability_line"])
    add(forge_runs[0]["storage_lifetime"])
    add(forge_runs[0]["sqlite_lifetime"])
    add("```")
    add("")
    add("Git configuration observed in the measured repository:")
    add("")
    add("```text")
    add(git_default[0]["git_version"])
    for mode, runs in (("default", git_default), ("durable", git_durable)):
        observed = runs[0]["durability_config_observed"] or ["(no fsync-related config set)"]
        for ln in observed:
            add("{}: {}".format(mode, ln))
    add("```")
    text = "\n".join(L) + "\n"
    path = os.path.join(out_dir, "w7-report.md")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)
    print(text)
    return 0



def forge_cli_env(repo):
    env = dict(os.environ)
    env["FORGE_DIR"] = repo
    env["FORGE_CAP"] = os.path.join(repo, ".forge", "keys", "root.cap")
    env.pop("LD_PRELOAD", None)
    return env


def forge_cli_agent(forge_bin, i, repo, env):
    """One logical W1 operation driven through the ForgeFS CLI: open a session
    pinned to main, write /w{i}.txt, check in.

    This configuration exists so that the comparison is losable. forge bench
    drives its agents as in-process threads while Git must exec twice per
    agent, and an orchestrator that shells out to forge pays exec cost too.
    Three execs here against two on the Git side, at ForgeFS durability."""

    def run(args):
        proc = subprocess.run(
            [forge_bin] + args,
            cwd=repo,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                "forge {} rc={} stderr={}".format(
                    " ".join(args),
                    proc.returncode,
                    proc.stderr.decode("utf-8", "replace"),
                )
            )
        return proc.stdout.decode("utf-8").strip()

    t0 = time.perf_counter_ns()
    ns = run(["session", "open", "--from", "main"])
    run(["write", "--ns", ns, "/w{}.txt".format(i), "--text", "agent {}".format(i)])
    out = run(["checkin", "--ns", ns, "-m", "bench"])
    elapsed = (time.perf_counter_ns() - t0) // 1000
    if not out.startswith("updated "):
        raise RuntimeError("checkin did not update a ref: {!r}".format(out))
    return elapsed


def cmd_run_forge_cli(args):
    root = os.path.abspath(args.root)
    if os.path.exists(root):
        raise RuntimeError("root path already exists: {}".format(root))
    os.makedirs(root)
    repo = os.path.join(root, "repo")
    os.makedirs(repo)
    subprocess.run(
        [args.forge_bin, "init", repo],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    env = forge_cli_env(repo)
    slots = [repo] * args.agents

    def op(i, target):
        return forge_cli_agent(args.forge_bin, i, target, env)

    latencies, wall = drive(op, slots, args.workers)

    fsck = subprocess.run(
        [args.forge_bin, "fsck", "--full"],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if fsck.returncode != 0:
        raise RuntimeError(
            "forge fsck --full failed: {}".format(fsck.stderr.decode("utf-8", "replace"))
        )

    out = {
        "side": "forgefs-cli",
        "workload": "W7 (ForgeFS W1 logical shape, one process per step)",
        "durability_mode": "synchronous=FULL, object file+dir fsync",
        "agents": args.agents,
        "workers": args.workers,
        "wall_s": wall,
        "ops_s": args.agents / wall if wall > 0 else 0.0,
        "latency": percentiles(latencies),
        "execs_per_agent_op": 3,
        "fsck": "ok",
    }
    text = json.dumps(out, indent=2, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            fh.write(text + "\n")
    else:
        print(text)
    if not args.keep:
        shutil.rmtree(root, ignore_errors=True)
    return 0


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    run = sub.add_parser("run", help="run the Git worktree workload")
    run.add_argument("--agents", type=int, required=True)
    run.add_argument("--workers", type=int, required=True)
    run.add_argument("--durability", choices=sorted(DURABILITY), required=True)
    run.add_argument("--root", required=True, help="new scratch path, must not exist")
    run.add_argument("--json-out")
    run.add_argument("--probe-lib", default="")
    run.add_argument("--keep", action="store_true")
    run.set_defaults(func=cmd_run)

    fc = sub.add_parser("run-forge-cli", help="same workload through the forge CLI")
    fc.add_argument("--forge-bin", required=True)
    fc.add_argument("--agents", type=int, required=True)
    fc.add_argument("--workers", type=int, required=True)
    fc.add_argument("--root", required=True, help="new scratch path, must not exist")
    fc.add_argument("--json-out")
    fc.add_argument("--keep", action="store_true")
    fc.set_defaults(func=cmd_run_forge_cli)

    verdict = sub.add_parser("verdict", help="apply the W7 durability gate")
    verdict.add_argument("--forge", required=True, help="ForgeFS barrier census JSON")
    verdict.add_argument("--git", required=True, help="Git run or barrier census JSON")
    verdict.set_defaults(func=cmd_verdict)

    pf = sub.add_parser("parse-forge", help="parse a `forge bench` report")
    pf.add_argument("--input", required=True)
    pf.add_argument("--workers", type=int, required=True)
    pf.add_argument("--json-out")
    pf.set_defaults(func=cmd_parse_forge)

    rep = sub.add_parser("report", help="render the W7 results section")
    rep.add_argument("--out-dir", required=True)
    rep.add_argument("--workers", type=int, required=True)
    rep.set_defaults(func=cmd_report)

    st = sub.add_parser("selftest", help="test the pure percentile and gate rules")
    st.set_defaults(func=cmd_selftest)

    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
