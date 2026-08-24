#!/usr/bin/env bash
# scripts/enospc-sigbus-probe.sh
#
# Characterisation harness for the near-full-filesystem SIGBUS band (issue #263).
#
# SQLite opens meta.sqlite with journal_mode=WAL, so it creates the wal-index
# .forge/meta.sqlite-shm, sparse-extends it to 32768 bytes with eight one-byte
# pwrite64 calls that all return 1, and then mmaps the whole region. On a
# filesystem whose block size is SMALLER than the CPU page size -- which
# mkfs.ext4 auto-selects (1024 or 2048) for images under 512 MB -- each one-byte
# write allocates only the final block of its 4096-byte page, so a 32 KiB
# mapping is backed by 8 KiB of blocks. A page fault into one of the holes on a
# filesystem that can no longer allocate is delivered as SIGBUS: the process
# dies with wait status 135, no exit code and an empty stderr, which no entry in
# CLI_ABI.md can describe.
#
# This harness asserts the contract that replaced that behaviour: at every free
# space level, every forge command must terminate with a documented exit code
# and must never be killed by a signal.
#
# Requires Linux, root (loop mount) and mkfs.ext4. It is deliberately NOT part
# of scripts/release-gate.sh, which must run unprivileged.
#
# Usage:
#   sudo scripts/enospc-sigbus-probe.sh <path-to-forge-binary> [image-mb]
#
# Exit status:
#   0  every op at every level returned a documented exit code
#   1  at least one op was killed by a signal, or a level could not be set up
#   2  harness failure: bad usage, not Linux, no root, missing mkfs.ext4
set -u

FORGE="${1:-}"
IMAGE_MB="${2:-16}"

harness_die() {
	printf 'enospc-sigbus-probe: harness error: %s\n' "$1" >&2
	exit 2
}

[ -n "$FORGE" ] || harness_die "usage: $0 <path-to-forge-binary> [image-mb]"
[ -x "$FORGE" ] || harness_die "not an executable forge binary: $FORGE"
[ "$(uname -s)" = Linux ] || harness_die "loop-mounted ext4 requires Linux"
[ "$(id -u)" = 0 ] || harness_die "must run as root to mount a loop device"
command -v mkfs.ext4 >/dev/null 2>&1 || harness_die "mkfs.ext4 is required"

FORGE="$(cd "$(dirname "$FORGE")" && pwd)/$(basename "$FORGE")"
WORK="$(mktemp -d)"
IMG="$WORK/ext4.img"
MNT="$WORK/mnt"
SRC="$WORK/src"
FAILURES=0

cleanup() {
	umount "$MNT" 2>/dev/null
	rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$MNT" "$SRC"
for i in $(seq 1 150); do printf 'content-%s\n' "$i" >"$SRC/f$i.txt"; done

avail_bytes() {
	echo $(($(stat -f -c %a "$MNT") * $(stat -f -c %S "$MNT")))
}

# The band is only reachable where the filesystem block size is smaller than the
# page size, so ask mkfs for 1024 explicitly rather than relying on the image
# being small enough for it to choose that on its own.
setup_fs() {
	umount "$MNT" 2>/dev/null
	rm -f "$IMG"
	dd if=/dev/zero of="$IMG" bs=1M count="$IMAGE_MB" status=none || return 1
	# -b 1024: the band exists only where the block size is below the page
	# size. -m 0: ext4 otherwise reserves 5% of blocks for root, and this
	# harness must run as root to mount, so a reserved pool would let every
	# allocation succeed past the point statvfs calls the filesystem full and
	# the band would never be entered.
	mkfs.ext4 -q -F -b 1024 -m 0 "$IMG" || return 1
	mount -o loop "$IMG" "$MNT" || return 1
	local page block
	page="$(getconf PAGESIZE)"
	block="$(stat -f -c %S "$MNT")"
	if [ "$block" -ge "$page" ]; then
		printf 'enospc-sigbus-probe: block size %s >= page size %s; band not reachable here\n' \
			"$block" "$page" >&2
		return 1
	fi
	return 0
}

# run_op <label> <argv...>: record the raw wait status without a pipeline, so a
# signal death is visible and SIGPIPE cannot be mistaken for one.
run_op() {
	local label="$1"
	shift
	"$@" >"$WORK/out" 2>&1
	local status=$?
	local first
	first="$(head -n1 "$WORK/out")"
	if [ "$status" -gt 128 ]; then
		printf '  %-6s KILLED by signal %s (wait status %s), stderr: %s\n' \
			"$label" "$((status - 128))" "$status" "${first:-<empty>}"
		FAILURES=$((FAILURES + 1))
		return
	fi
	case "$status" in
	0 | 1 | 2 | 3 | 4 | 5)
		printf '  %-6s exit %s  %s\n' "$label" "$status" "$first"
		;;
	*)
		printf '  %-6s exit %s NOT IN CLI_ABI.md  %s\n' "$label" "$status" "$first"
		FAILURES=$((FAILURES + 1))
		;;
	esac
}

# Levels straddle the measured band: above it every op succeeds, inside it the
# unfixed binary is killed by SIGBUS, below it SQLite already fails cleanly.
for target in 262144 65536 40960 32768 24576 16384 10240 9216 8192 4096 1024; do
	if ! setup_fs; then
		printf 'enospc-sigbus-probe: could not prepare filesystem\n' >&2
		exit 1
	fi
	repo="$MNT/repo"
	"$FORGE" init "$repo" >/dev/null 2>&1 || {
		printf 'enospc-sigbus-probe: init failed\n' >&2
		exit 1
	}
	cap="$repo/.forge/keys/root.cap"
	"$FORGE" --dir "$repo" --cap "$cap" branch main work >/dev/null 2>&1
	"$FORGE" --dir "$repo" --cap "$cap" import --ref work "$SRC" >/dev/null 2>&1
	sync
	fill=$(($(avail_bytes) - target))
	if [ "$fill" -gt 1024 ]; then
		dd if=/dev/zero of="$MNT/filler" bs=1024 count=$((fill / 1024)) status=none 2>/dev/null
		sync
	fi
	printf 'free=%s (target %s)\n' "$(avail_bytes)" "$target"
	run_op refs "$FORGE" --dir "$repo" --cap "$cap" refs
	run_op log "$FORGE" --dir "$repo" --cap "$cap" log work
	run_op fsck "$FORGE" --dir "$repo" --cap "$cap" fsck --full
	run_op seal "$FORGE" --dir "$repo" --cap "$cap" seal --tag v1 work
done

if [ "$FAILURES" -ne 0 ]; then
	printf 'enospc-sigbus-probe: FAIL (%s ops did not return a documented exit code)\n' "$FAILURES"
	exit 1
fi
printf 'enospc-sigbus-probe: PASS\n'
