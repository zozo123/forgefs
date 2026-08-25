#!/usr/bin/env bash
# scripts/w7-git-comparator.sh
#
# The checked-in W7 comparator required by issue #24 and docs/BENCH.md.
#
# It runs ONE logical workload -- N agents, each making one small edit and
# committing it onto its own ref -- against three configurations:
#
#   1. ForgeFS       forge bench W1 private checkins: SQLite synchronous=FULL,
#                    object file and directory fsync. In-process threads.
#   1b. forge CLI    the same ForgeFS work, three forge execs per agent, so
#                    that a shell-out orchestrator is compared like for like
#                    with git and the comparison can actually be lost.
#   2. git-default   N git worktrees at the durability this git actually ships
#                    with. That is what a user gets by typing git commit, and
#                    it is not ForgeFS durability.
#   3. git-durable   the same worktrees with core.fsync=all and
#                    core.fsyncMethod=fsync: every barrier git knows to make.
#
# All three numbers are always reported. Git at its default is included
# because it honestly describes the tool people compare against; it is never
# presented as an equal-durability result.
#
# Before any ForgeFS/Git quotient is printed as a ratio, the script measures
# the durability barriers each path actually issues for one agent operation
# (scripts/w7_fsync_probe.c) and applies the docs/BENCH.md W7 gate
# (classify_equivalence in scripts/w7_git_worktree_bench.py). If equivalence
# is not demonstrated the raw numbers are still published and the ratio is
# labelled non-comparable: durability mismatch/unknown. That outcome is a
# normal result of this script, not a failure of it.
#
# Usage:
#   scripts/w7-git-comparator.sh [--forge PATH] [--agents N] [--workers W]
#                                [--reps R] [--out DIR]
#
#   --forge PATH   forge binary (default: target/release/forge)
#   --agents N     agents per repetition (default: 32)
#   --workers W    bounded worker count (default: 4)
#   --reps R       fresh-repository repetitions per configuration (default: 5,
#                  the docs/BENCH.md minimum for a published claim)
#   --out DIR      results directory; must not already exist
#
# Writes DIR/w7-report.md plus every unedited per-repetition JSON, because
# docs/BENCH.md requires all repetitions and not just the best one.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
harness="$here/w7_git_worktree_bench.py"
probe_src="$here/w7_fsync_probe.c"
env_line_script="$here/forge-env-line.sh"

forge_bin="$repo_root/target/release/forge"
agents=32
workers=4
reps=5
out_dir=""

die() {
	printf "w7-git-comparator: %s\n" "$1" >&2
	exit 2
}

while [ $# -gt 0 ]; do
	case "$1" in
	--forge) forge_bin="${2:-}"; shift 2 ;;
	--agents) agents="${2:-}"; shift 2 ;;
	--workers) workers="${2:-}"; shift 2 ;;
	--reps) reps="${2:-}"; shift 2 ;;
	--out) out_dir="${2:-}"; shift 2 ;;
	-h|--help) sed -n "2,41p" "${BASH_SOURCE[0]}"; exit 0 ;;
	*) die "unknown argument: $1" ;;
	esac
done

command -v git >/dev/null 2>&1 || die "git not found"
command -v python3 >/dev/null 2>&1 || die "python3 not found"
if [ ! -x "$forge_bin" ]; then
	die "forge binary not found or not executable: $forge_bin"
fi
forge_bin="$(cd "$(dirname "$forge_bin")" && pwd)/$(basename "$forge_bin")"

if [ -z "$out_dir" ]; then
	out_dir="$(mktemp -d -t w7-comparator-XXXXXX)"
else
	if [ -e "$out_dir" ]; then
		die "results directory already exists: $out_dir"
	fi
	mkdir -p "$out_dir"
fi
out_dir="$(cd "$out_dir" && pwd)"
work_dir="$out_dir/work"
mkdir -p "$work_dir"

# The pure rules the report depends on are tested before anything is measured.
python3 "$harness" selftest >"$out_dir/selftest.txt" || die "harness selftest failed"

# --- durability barrier probe --------------------------------------------
# Optional, Linux/glibc only. Without it the gate reports durability unknown
# and refuses the ratio, which is the correct answer rather than a fallback.
probe_lib=""
if command -v cc >/dev/null 2>&1; then
	if cc -shared -fPIC -O2 -o "$work_dir/w7_fsync_probe.so" "$probe_src" -ldl \
		>"$work_dir/probe-build.log" 2>&1; then
		probe_lib="$work_dir/w7_fsync_probe.so"
	fi
fi
if [ -z "$probe_lib" ]; then
	printf "w7-git-comparator: barrier probe unavailable; the ratio will be reported non-comparable\n" >&2
fi

# ForgeFS barrier census for one agent operation: write + checkin, matching
# the git side add + commit. Repository creation, capability grant and
# session open are excluded on both sides.
#
# Scope note, stated because it matters: these two forge CLI processes each
# perform their own open-time object/tmp directory durability setup, so this
# count is an upper bound on one in-process checkin. The gate reads only
# presence or absence of a barrier class, which that overcount cannot flip.
probe_repo="$work_dir/forge-probe"
"$forge_bin" init "$probe_repo" >"$work_dir/forge-probe-init.log" 2>&1
export FORGE_DIR="$probe_repo"
export FORGE_CAP="$probe_repo/.forge/keys/root.cap"
ns="$("$forge_bin" session open --from main)"
forge_barriers="$out_dir/forge-barriers.json"
if [ -n "$probe_lib" ]; then
	barrier_log="$work_dir/forge-barriers.log"
	: >"$barrier_log"
	W7_FSYNC_LOG="$barrier_log" LD_PRELOAD="$probe_lib" \
		"$forge_bin" write --ns "$ns" /w0.txt --text "agent 0" >/dev/null
	W7_FSYNC_LOG="$barrier_log" LD_PRELOAD="$probe_lib" \
		"$forge_bin" checkin --ns "$ns" -m bench >/dev/null
	file_barriers="$(grep -c " file$" "$barrier_log" || true)"
	dir_barriers="$(grep -c " dir$" "$barrier_log" || true)"
	cat >"$forge_barriers" <<JSON
{
  "available": true,
  "file": ${file_barriers:-0},
  "dir": ${dir_barriers:-0},
  "scope": "forge write + forge checkin for one agent (two CLI processes)"
}
JSON
else
	cat >"$forge_barriers" <<JSON
{
  "available": false,
  "reason": "barrier probe not built"
}
JSON
fi

# --- required environment line -------------------------------------------
FORGE_ENV_COMMAND="$forge_bin bench --agents $agents --shared 0 --workers $workers ;; w7_git_worktree_bench.py run --agents $agents --workers $workers --durability {default,durable}" \
	FORGE_ENV_WORKERS="$workers" \
	FORGE_ENV_PROFILE="${FORGE_ENV_PROFILE:-release}" \
	FORGE_ENV_RUN_CLASS="${FORGE_ENV_RUN_CLASS:-cold}" \
	FORGE_ENV_REPO_CLASS="${FORGE_ENV_REPO_CLASS:-fresh repository per repetition, both sides}" \
	"$env_line_script" --forge "$forge_bin" "$probe_repo" >"$out_dir/env-line.txt" 2>"$work_dir/env-line.err" ||
	printf "environment line unavailable\n" >"$out_dir/env-line.txt"
unset FORGE_DIR FORGE_CAP

# --- measurement ----------------------------------------------------------
# W7 is a W1-only comparison, so --shared 0: no shared-ref stampede here.
rep=1
while [ "$rep" -le "$reps" ]; do
	raw="$out_dir/forge-rep$rep.txt"
	if ! "$forge_bin" bench --agents "$agents" --shared 0 --workers "$workers" >"$raw" 2>&1; then
		cat "$raw" >&2
		die "forge bench failed on repetition $rep"
	fi
	python3 "$harness" parse-forge --input "$raw" --workers "$workers" \
		--json-out "$out_dir/forge-rep$rep.json" >/dev/null

	python3 "$harness" run-forge-cli \
		--forge-bin "$forge_bin" --agents "$agents" --workers "$workers" \
		--root "$work_dir/forgecli-rep$rep" \
		--json-out "$out_dir/forgecli-rep$rep.json" >/dev/null

	for mode in default durable; do
		python3 "$harness" run \
			--agents "$agents" --workers "$workers" --durability "$mode" \
			--root "$work_dir/git-$mode-rep$rep" \
			--probe-lib "$probe_lib" \
			--json-out "$out_dir/git-$mode-rep$rep.json" >/dev/null
	done
	rep=$((rep + 1))
done

# --- report ---------------------------------------------------------------
python3 "$harness" report --out-dir "$out_dir" --workers "$workers"
printf "\nw7-git-comparator: raw per-repetition results in %s\n" "$out_dir" >&2
