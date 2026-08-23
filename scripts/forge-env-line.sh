#!/usr/bin/env bash
# scripts/forge-env-line.sh
#
# Emit the "required environment line" that docs/BENCH.md demands for every
# published run, and that scripts/release-gate.sh records for every release.
#
# Usage:
#   scripts/forge-env-line.sh [--json] [--forge PATH] [--meta PATH] [DIR]
#
#   --json        emit machine-readable JSON instead of the human-readable line
#   --forge PATH  a `forge` binary to interrogate for `forge --version`
#   --meta PATH   an explicit .forge/meta.sqlite to observe
#   DIR           directory whose filesystem/storage is reported (default: .)
#                 if DIR/.forge/meta.sqlite exists it is observed automatically
#
# Optional environment overrides (all recorded verbatim):
#   FORGE_ENV_COMMAND      exact command line the measurement ran
#   FORGE_ENV_WORKERS      worker count for the measured command
#   FORGE_ENV_PROFILE      build profile (default: release)
#   FORGE_ENV_COMMIT       commit sha (default: git rev-parse HEAD, else unknown)
#   FORGE_ENV_RUN_CLASS    cold | warm | both (default: cold)
#   FORGE_ENV_REPETITION   repetition number (default: 1)
#   FORGE_ENV_REPO_CLASS   fresh-repository path/class description
#
# Honesty rule, per docs/BENCH.md: a field this script cannot actually observe
# is rendered as `unavailable` or explicitly marked `declared`. It is never
# guessed and never silently omitted.
set -euo pipefail

emit_json=0
forge_bin=""
meta_path=""
target_dir="."

die() {
	printf 'forge-env-line: %s\n' "$1" >&2
	exit 2
}

# journal_mode is persistent in the database file, so it is genuinely
# observable from outside the process that wrote it.
probe_journal_mode() {
	FORGE_ENV_META_PATH="$1" python3 - <<'PY'
import os
import sqlite3
import urllib.parse

path = os.path.abspath(os.environ["FORGE_ENV_META_PATH"])
try:
    uri = "file:" + urllib.parse.quote(path) + "?mode=ro"
    con = sqlite3.connect(uri, uri=True)
    print(str(con.execute("PRAGMA journal_mode").fetchone()[0]).upper())
    con.close()
except Exception:
    print("unavailable")
PY
}

emit_json_doc() {
	python3 - <<'PY'
import json
import os

out = {k[4:]: v for k, v in os.environ.items() if k.startswith("FEL_")}
print(json.dumps(out, indent=2, sort_keys=True))
PY
}

while [ "$#" -gt 0 ]; do
	case "$1" in
	--json)
		emit_json=1
		shift
		;;
	--forge)
		[ "$#" -ge 2 ] || die "--forge needs a path"
		forge_bin="$2"
		shift 2
		;;
	--meta)
		[ "$#" -ge 2 ] || die "--meta needs a path"
		meta_path="$2"
		shift 2
		;;
	-h | --help)
		sed -n '2,28p' "$0"
		exit 0
		;;
	--)
		shift
		break
		;;
	-*)
		die "unknown option $1"
		;;
	*)
		target_dir="$1"
		shift
		;;
	esac
done

[ -d "$target_dir" ] || die "not a directory: $target_dir"
if [ -z "$meta_path" ] && [ -f "$target_dir/.forge/meta.sqlite" ]; then
	meta_path="$target_dir/.forge/meta.sqlite"
fi

uname_s="$(uname -s)"
uname_m="$(uname -m)"
kernel="$(uname -sr)"

cpu_model="unavailable"
cpu_cores="unavailable"
ram_bytes="unavailable"
os_name="$uname_s"
os_version="unavailable"
filesystem="unavailable"
storage="unavailable"
fullfsync="n/a (Linux fsync path, docs/RECOVERY.md)"

case "$uname_s" in
Darwin)
	cpu_model="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unavailable)"
	cpu_cores="$(sysctl -n hw.logicalcpu 2>/dev/null || echo unavailable)"
	ram_bytes="$(sysctl -n hw.memsize 2>/dev/null || echo unavailable)"
	os_name="macOS"
	os_version="$(sw_vers -productVersion 2>/dev/null || echo unavailable)"
	# `df` names the device; `mount` names that device's filesystem type.
	dev="$(df -P "$target_dir" 2>/dev/null | awk 'NR==2 {print $1}' || true)"
	if [ -n "${dev:-}" ]; then
		filesystem="$(mount 2>/dev/null | awk -v d="$dev" '$1==d {
			for (i = 1; i <= NF; i++) {
				if ($i ~ /^\(/) { gsub(/[(),]/, "", $i); print $i; exit }
			}
		}' || true)"
		storage="$dev"
	fi
	[ -n "${filesystem:-}" ] || filesystem="unavailable"
	fullfsync="ON (declared; Meta::open fails closed without it, docs/RECOVERY.md)"
	;;
Linux)
	cpu_model="$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null || true)"
	if [ -z "${cpu_model:-}" ]; then
		cpu_model="$(lscpu 2>/dev/null | awk -F': +' '/^Model name/ {print $2; exit}' || true)"
	fi
	[ -n "${cpu_model:-}" ] || cpu_model="unavailable"
	cpu_cores="$(nproc 2>/dev/null || echo unavailable)"
	mem_kb="$(awk '/^MemTotal:/ {print $2; exit}' /proc/meminfo 2>/dev/null || true)"
	if [ -n "${mem_kb:-}" ]; then
		ram_bytes="$((mem_kb * 1024))"
	fi
	if [ -r /etc/os-release ]; then
		os_name="$(awk -F= '/^NAME=/ {gsub(/"/, "", $2); print $2; exit}' /etc/os-release)"
		os_version="$(awk -F= '/^VERSION_ID=/ {gsub(/"/, "", $2); print $2; exit}' /etc/os-release)"
		[ -n "${os_name:-}" ] || os_name="Linux"
		[ -n "${os_version:-}" ] || os_version="unavailable"
	fi
	filesystem="$(stat -f -c %T "$target_dir" 2>/dev/null || echo unavailable)"
	src="$(findmnt -no SOURCE --target "$target_dir" 2>/dev/null || true)"
	if [ -n "${src:-}" ]; then
		model="$(lsblk -no MODEL "$src" 2>/dev/null | head -n1 | tr -s ' ' || true)"
		storage="$src${model:+ ($model)}"
	fi
	;;
esac

ram_human="unavailable"
if [ "$ram_bytes" != "unavailable" ]; then
	ram_human="$(awk -v b="$ram_bytes" 'BEGIN {printf "%.1f GiB", b / 1073741824}')"
fi

journal_mode="unavailable (no meta.sqlite probed)"
if [ -n "$meta_path" ] && [ -f "$meta_path" ]; then
	journal_mode="$(probe_journal_mode "$meta_path" 2>/dev/null || echo unavailable)"
fi
# synchronous and fullfsync are per-connection, not stored in the file, so they
# are reported as declared policy enforced by Meta::open, never as observed.
synchronous="FULL (declared; Meta::open fails closed without it, docs/RECOVERY.md)"

forge_version="unavailable"
if [ -n "$forge_bin" ] && [ -x "$forge_bin" ]; then
	forge_version="$("$forge_bin" --version 2>/dev/null | head -n1 || echo unavailable)"
fi

rustc_version="unavailable"
if command -v rustc >/dev/null 2>&1; then
	rustc_version="$(rustc --version 2>/dev/null || echo unavailable)"
fi

commit="${FORGE_ENV_COMMIT:-}"
if [ -z "$commit" ]; then
	commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
fi

profile="${FORGE_ENV_PROFILE:-release}"
command_line="${FORGE_ENV_COMMAND:-unavailable}"
workers="${FORGE_ENV_WORKERS:-unavailable}"
run_class="${FORGE_ENV_RUN_CLASS:-cold}"
repetition="${FORGE_ENV_REPETITION:-1}"
repo_class="${FORGE_ENV_REPO_CLASS:-$target_dir (fresh repository per invocation)}"

if [ "$emit_json" -eq 1 ]; then
	FEL_commit="$commit" \
		FEL_build_profile="$profile" \
		FEL_forge_version="$forge_version" \
		FEL_rustc="$rustc_version" \
		FEL_command="$command_line" \
		FEL_workers="$workers" \
		FEL_cpu_model="$cpu_model" \
		FEL_cpu_logical_cores="$cpu_cores" \
		FEL_ram_bytes="$ram_bytes" \
		FEL_ram_human="$ram_human" \
		FEL_os_name="$os_name" \
		FEL_os_version="$os_version" \
		FEL_kernel="$kernel" \
		FEL_arch="$uname_m" \
		FEL_filesystem="$filesystem" \
		FEL_storage_device="$storage" \
		FEL_sqlite_journal_mode="$journal_mode" \
		FEL_sqlite_synchronous="$synchronous" \
		FEL_fullfsync="$fullfsync" \
		FEL_run_class="$run_class" \
		FEL_repetition="$repetition" \
		FEL_repo_class="$repo_class" \
		emit_json_doc
	exit 0
fi

cat <<EOF
forgefs commit:        $commit
build profile:         $profile
forge --version:       $forge_version
rustc:                 $rustc_version
command line:          $command_line
worker count:          $workers
cpu model:             $cpu_model
cpu logical cores:     $cpu_cores
ram:                   $ram_human ($ram_bytes bytes)
os:                    $os_name $os_version
kernel:                $kernel
arch:                  $uname_m
filesystem:            $filesystem
storage device:        $storage
sqlite journal_mode:   $journal_mode
sqlite synchronous:    $synchronous
macos fullfsync:       $fullfsync
run class:             $run_class
repetition:            $repetition
repository class:      $repo_class
EOF
