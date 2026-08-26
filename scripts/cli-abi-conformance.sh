#!/usr/bin/env bash
# scripts/cli-abi-conformance.sh
#
# Executable conformance test for the CLI exit-code ABI in CLI_ABI.md.
# Issue #237: "exit 5 reachable from caller-controlled input; CLI_ABI.md has no
# conformance test". This is that test.
#
# Usage:
#   scripts/cli-abi-conformance.sh <path-to-forge-binary> [OUTDIR]
#
# OUTDIR defaults to ./abi-conformance-out and receives:
#   abi-conformance.json   one record per row: argv, expected, observed, class
#
# Exit status:
#   0  every BLOCKING row matched the contract
#   1  at least one BLOCKING row did not match the contract
#   2  harness failure (bad usage, fixture could not be built)
#   3  a prerequisite this harness needs is missing; the message names it
#
# Prerequisites, in full: bash, coreutils, sed, awk, and the `forge` binary
# under test. That is the whole list, and it is deliberate: this script and
# scripts/release-gate.sh are what a cautious user runs before trusting a
# release, so they must have FEWER prerequisites than the product, not more.
# They used to demand `python3` -- for JSON shaping and one byte-fill -- and
# refused to start without it with exit 2, the code CLI_ABI.md reserves for
# corruption (issue #346). JSON is now shaped by scripts/json-lib.sh.
#
# The list is checked, not just claimed: scripts/prereq-lib.sh names every
# command these scripts may run and verifies all of them before the first row,
# on every shell. It was claimed and false until issue #354 -- `grep` is its
# own package, and release-gate.sh used it while all three declarations said
# otherwise. On bash >= 4 an undeclared command that a script reaches for
# anyway is additionally turned into the same exit 3 at the point of use; that
# backstop needs `command_not_found_handle` and so does not exist on bash 3.2.
#
# ---------------------------------------------------------------------------
# What "expected" means here
# ---------------------------------------------------------------------------
# Every row encodes the CONTRACT from CLI_ABI.md, never today's observed
# behaviour:
#
#   0  success
#   1  denied/capability/input/not-found
#   2  corruption or sealed-state violation
#   3  transient busy/contention
#   4  stale observation or merge conflict
#   5  I/O, SQLite, or internal failure
#
# Rows whose contract value ForgeFS does not yet honour live in the
# `known_failing` class. They are executed and REPORTED, but they do not fail
# this script - so the release is not held hostage to an open bug - and they
# flip to blocking automatically the moment the bug is fixed, because a
# known_failing row that starts matching its contract is itself reported as a
# hard error ("stale known_failing row"). That is what makes the marker
# self-cleaning instead of a permanent excuse.
set -euo pipefail

FORGE="${1:-}"
OUTDIR="${2:-./abi-conformance-out}"

die() {
	printf 'abi-conformance: %s\n' "$1" >&2
	exit 2
}

[ -n "$FORGE" ] || die "usage: $0 <path-to-forge-binary> [OUTDIR]"
[ -x "$FORGE" ] || die "not an executable forge binary: $FORGE"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ -r "$SCRIPT_DIR/prereq-lib.sh" ] || {
	printf 'abi-conformance: missing %s\n' "$SCRIPT_DIR/prereq-lib.sh" >&2
	exit 3
}
PREREQ_SCRIPT=abi-conformance
# shellcheck source=scripts/prereq-lib.sh
. "$SCRIPT_DIR/prereq-lib.sh"
# A declared tool this machine does not have is a harness error with an exit
# code of its own, decided before the first row runs -- never a contract row
# that appears to have failed (issue #354).
require_declared_commands
[ -r "$SCRIPT_DIR/json-lib.sh" ] || {
	printf 'abi-conformance: missing %s\n' "$SCRIPT_DIR/json-lib.sh" >&2
	exit 3
}
# shellcheck source=scripts/json-lib.sh
. "$SCRIPT_DIR/json-lib.sh"
FORGE="$(cd "$(dirname "$FORGE")" && pwd)/$(basename "$FORGE")"

mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"

# No ambient authority and no ambient repository may leak in from the caller's
# shell; several rows assert precisely the absence of a capability.
unset FORGE_CAP FORGE_DIR

WORK="$(mktemp -d "${TMPDIR:-/tmp}/forge-abi.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
# From here on a command bash cannot find is recorded, and `check` refuses to
# record a contract row as failed because of it.
PREREQ_MARKER="$WORK/missing-commands"
# `forge` walks parents looking for .forge, so no fixture may be created at
# $WORK itself - otherwise the "no repository here" row would find a parent.

ROWS_JSON="$WORK/rows.json"
KNOWN_FAILING_JSON="$WORK/known-failing.json"
: >"$ROWS_JSON"
: >"$KNOWN_FAILING_JSON"

blocking_fail=0
row_count=0
rows_blocking=0
rows_known_failing=0
rows_unexercised=0
rows_first=1
known_failing_first=1

# Capability tokens ARE authority (INVARIANTS I13/I14). Even fixture tokens are
# never written to a log line or a CI artifact.
redact() {
	sed -e 's/fmac1_[0-9a-fA-F]*/fmac1_<redacted-capability-token>/g'
}

# record <id> <class> <expected> <observed> <argv-string> <note> <output>
#
# Appends this row to the report as it happens, rather than buffering a
# delimited record for a second pass to re-parse. The verdict is recomputed
# here from class/expected/observed, so the artifact says what the row DID,
# independently of how the console line above chose to describe it.
record() {
	local id="$1" class="$2" expected="$3" observed="$4" argv="$5" note="$6" out="$7"
	local verdict observed_json

	if [ "$class" = unexercised ]; then
		verdict=unexercised
		observed_json=null
		rows_unexercised=$((rows_unexercised + 1))
	else
		observed_json="$observed"
		if [ "$observed" -eq "$expected" ]; then
			verdict=match
		else
			verdict=mismatch
		fi
		if [ "$class" = blocking ]; then
			rows_blocking=$((rows_blocking + 1))
		else
			rows_known_failing=$((rows_known_failing + 1))
		fi
	fi

	{
		[ "$rows_first" -eq 1 ] || printf ',\n'
		printf '    {\n'
		json_field 6 argv "$argv" ,
		json_field 6 class "$class" ,
		json_raw_field 6 contract_exit "$expected" ,
		json_field 6 id "$id" ,
		json_field 6 note "$note" ,
		json_raw_field 6 observed_exit "$observed_json" ,
		json_field 6 output "$out" ,
		json_field 6 verdict "$verdict"
		printf '    }'
	} >>"$ROWS_JSON"
	rows_first=0

	# scripts/release-gate.sh reprints the known_failing rows in its own
	# summary. Handing it a ready-made fragment is what keeps either script
	# from having to parse the other's JSON back in.
	if [ "$class" = known_failing ]; then
		{
			[ "$known_failing_first" -eq 1 ] || printf ',\n'
			printf '      {\n'
			json_raw_field 8 contract_exit "$expected" ,
			json_field 8 id "$id" ,
			json_field 8 note "$note" ,
			json_raw_field 8 observed_exit "$observed_json"
			printf '      }'
		} >>"$KNOWN_FAILING_JSON"
		known_failing_first=0
	fi
}

# check <id> <class:blocking|known_failing> <expected-exit> <note> -- <forge argv...>
check() {
	local id="$1" class="$2" expected="$3" note="$4"
	shift 4
	[ "${1:-}" = "--" ] && shift
	local out observed argv
	argv="$(printf 'forge %s' "$*" | redact)"
	set +e
	out="$("$FORGE" "$@" 2>&1)"
	observed=$?
	set -e
	out="$(printf '%s' "$out" | redact)"
	row_count=$((row_count + 1))

	local verdict
	if [ "$observed" -eq "$expected" ]; then
		verdict="match"
	else
		verdict="mismatch"
	fi

	case "$class:$verdict" in
	blocking:match)
		printf 'ok       %-34s exit=%s  %s\n' "$id" "$observed" "$argv"
		;;
	blocking:mismatch)
		# A row can only disprove the contract if the harness itself ran.
		prereq_guard
		printf 'FAIL     %-34s expected=%s observed=%s  %s\n' "$id" "$expected" "$observed" "$argv" >&2
		printf '         output: %s\n' "$(printf '%s' "$out" | tr '\n' '|')" >&2
		blocking_fail=$((blocking_fail + 1))
		;;
	known_failing:mismatch)
		# Expected-to-be-broken today. Reported, non-blocking.
		printf 'KNOWN    %-34s contract=%s observed=%s  %s   [%s]\n' \
			"$id" "$expected" "$observed" "$argv" "$note"
		;;
	known_failing:match)
		# The bug named in $note is fixed. Promote the row to blocking.
		printf 'STALE    %-34s contract=%s observed=%s  %s\n' "$id" "$expected" "$observed" "$argv" >&2
		printf '         known_failing row now honours the contract: %s\n' "$note" >&2
		printf '         move it to the blocking set and delete the marker.\n' >&2
		blocking_fail=$((blocking_fail + 1))
		verdict="stale-known-failing"
		;;
	esac
	record "$id" "$class" "$expected" "$observed" "$argv" "$note" "$out"
}

# A row that cannot be produced deterministically is declared, not faked.
declare_unexercised() {
	local id="$1" expected="$2" note="$3"
	row_count=$((row_count + 1))
	printf 'SKIP     %-34s contract=%s  [%s]\n' "$id" "$expected" "$note"
	record "$id" "unexercised" "$expected" "-1" "-" "$note" ""
}

run() {
	"$FORGE" "$@" >/dev/null 2>&1 || die "fixture step failed: forge $*"
}
capture() {
	"$FORGE" "$@" 2>/dev/null || die "fixture step failed: forge $*"
}

# ---------------------------------------------------------------------------
# Fixture A: a healthy forge with two agents, a merge and a sealed tag.
# ---------------------------------------------------------------------------
A="$WORK/fixture-healthy"
mkdir -p "$A"
run init "$A"
A_ROOT="$A/.forge/keys/root.cap"
A_INT="$A/.forge/keys/integrator.cap"
[ -f "$A_ROOT" ] && [ -f "$A_INT" ] || die "init did not produce root/integrator caps"

A_ALICE="$(capture --dir "$A" --cap "$A_ROOT" grant --ops read,write,branch \
	--ref 'main,heads/agents/alice/*' --agent alice | tr -d '\n')"
A_NS="$(capture --dir "$A" --cap "$A_ROOT" session open --from=main | tr -d '\n')"
run --dir "$A" --cap "$A_ROOT" write --ns "$A_NS" /abi.txt --text abi
run --dir "$A" --cap "$A_ROOT" checkin --ns "$A_NS" -m abi
run --dir "$A" --cap "$A_INT" merge --into=main --from "heads/agents/anon/$A_NS"
run --dir "$A" --cap "$A_INT" seal main --tag abi-seal --attest
# Precondition for the duplicate-branch row below.
run --dir "$A" --cap "$A_ROOT" branch main heads/abi-dup

# Precondition for the `forge mv` rows below: a session of its own, so the move
# rows never perturb the overlay the other fixture-A rows depend on.
A_MVNS="$(capture --dir "$A" --cap "$A_ROOT" session open --from=main | tr -d '\n')"

A_NONUTF8="$WORK/non-utf8.cap"
printf '\377\376bad' >"$A_NONUTF8"

# ---------------------------------------------------------------------------
# Fixture B: same-path overlap, so a second merge must conflict.
# ---------------------------------------------------------------------------
B="$WORK/fixture-conflict"
mkdir -p "$B"
run init "$B"
B_ROOT="$B/.forge/keys/root.cap"
B_INT="$B/.forge/keys/integrator.cap"
B_A="$(capture --dir "$B" --cap "$B_ROOT" session open --from=main | tr -d '\n')"
B_B="$(capture --dir "$B" --cap "$B_ROOT" session open --from=main | tr -d '\n')"
run --dir "$B" --cap "$B_ROOT" write --ns "$B_A" /same.txt --text ours
run --dir "$B" --cap "$B_ROOT" write --ns "$B_B" /same.txt --text theirs
run --dir "$B" --cap "$B_ROOT" checkin --ns "$B_A" -m ours
run --dir "$B" --cap "$B_ROOT" checkin --ns "$B_B" -m theirs
run --dir "$B" --cap "$B_INT" merge --into=main --from "heads/agents/anon/$B_A"

# ---------------------------------------------------------------------------
# Fixture C: a stale observation, so a disjoint checkin must fail.
# ---------------------------------------------------------------------------
C="$WORK/fixture-stale"
mkdir -p "$C"
run init "$C"
C_ROOT="$C/.forge/keys/root.cap"
C_INT="$C/.forge/keys/integrator.cap"
C_ALICE="$(capture --dir "$C" --cap "$C_ROOT" grant --ops read,write,branch \
	--ref 'main,heads/agents/alice/*' --agent alice | tr -d '\n')"
C_BOB="$(capture --dir "$C" --cap "$C_ROOT" grant --ops read,write,branch \
	--ref 'main,heads/agents/bob/*' --agent bob | tr -d '\n')"
C_A1="$(capture --dir "$C" --cap "$C_ALICE" session open --from=main | tr -d '\n')"
run --dir "$C" --cap "$C_ALICE" write --ns "$C_A1" /doc.txt --text v1
run --dir "$C" --cap "$C_ALICE" checkin --ns "$C_A1" -m v1
run --dir "$C" --cap "$C_INT" merge --into=main --from "heads/agents/alice/$C_A1"
C_BOBNS="$(capture --dir "$C" --cap "$C_BOB" session open --from=main | tr -d '\n')"
run --dir "$C" --cap "$C_BOB" read --ns "$C_BOBNS" /main/doc.txt
C_A2="$(capture --dir "$C" --cap "$C_ALICE" session open --from=main | tr -d '\n')"
run --dir "$C" --cap "$C_ALICE" write --ns "$C_A2" /doc.txt --text v2
run --dir "$C" --cap "$C_ALICE" checkin --ns "$C_A2" -m v2
run --dir "$C" --cap "$C_INT" merge --into=main --from "heads/agents/alice/$C_A2"
run --dir "$C" --cap "$C_BOB" write --ns "$C_BOBNS" /notes.txt --text notes

# ---------------------------------------------------------------------------
# Fixture D: durable bitrot, so fsck must fail closed as corruption.
# ---------------------------------------------------------------------------
D="$WORK/fixture-bitrot"
mkdir -p "$D"
run init "$D"
D_ROOT="$D/.forge/keys/root.cap"
D_NS="$(capture --dir "$D" --cap "$D_ROOT" session open --from=main | tr -d '\n')"
D_BLOB="$(capture --dir "$D" --cap "$D_ROOT" write --ns "$D_NS" /rot.txt --text 'immutable bytes' | tr -d '\n')"
[ "${#D_BLOB}" -eq 64 ] || die "unexpected ObjectId from write: $D_BLOB"
run --dir "$D" --cap "$D_ROOT" checkin --ns "$D_NS" -m rot
D_OBJ="$D/.forge/objects/${D_BLOB:0:2}/${D_BLOB:2:2}/$D_BLOB"
[ -f "$D_OBJ" ] || die "blob object missing at $D_OBJ"
# Overwrite the object's bytes in place, keeping its length, so the ObjectId in
# its path no longer describes its content. `head -c` from /dev/zero is the
# whole trick, and it is why this fixture no longer needs an interpreter
# (issue #346). Objects are stored read-only, so make it writable first.
D_SIZE="$(wc -c <"$D_OBJ" | tr -d ' ')"
[ "$D_SIZE" -gt 0 ] || die "blob object is empty at $D_OBJ"
chmod u+w "$D_OBJ" || die "could not make $D_OBJ writable"
head -c "$D_SIZE" /dev/zero >"$D_OBJ" || die "could not overwrite $D_OBJ"
[ "$(wc -c <"$D_OBJ" | tr -d ' ')" -eq "$D_SIZE" ] ||
	die "bitrot fixture must not change the object's length"

# ---------------------------------------------------------------------------
# Fixture E: a directory that is not a repository and has no repository above.
# ---------------------------------------------------------------------------
E="$WORK/fixture-not-a-repo/deep"
mkdir -p "$E"

# ---------------------------------------------------------------------------
# Fixture F: a throwaway forge for the #237 rows that mutate metadata.
# These live in their own fixture on purpose: some of them leave the
# repository in a state `fsck` calls corruption, and no other row may inherit
# that.
# ---------------------------------------------------------------------------
F="$WORK/fixture-input-abi"
mkdir -p "$F"
run init "$F"
F_ROOT="$F/.forge/keys/root.cap"
F_NS="$(capture --dir "$F" --cap "$F_ROOT" session open --from=main | tr -d '\n')"
F_PLAIN_FILE="$WORK/not-a-directory"
: >"$F_PLAIN_FILE"
F_MISSING_FILE="$WORK/definitely-absent.bin"

# ---------------------------------------------------------------------------
# Fixture G: a contended round that forked, plus a session holding staged work.
# This is the #12 / #309 surface: `abandon` and `gc` exit codes.
# ---------------------------------------------------------------------------
G="$WORK/fixture-fork"
mkdir -p "$G"
run init "$G"
G_ROOT="$G/.forge/keys/root.cap"
run --dir "$G" --cap "$G_ROOT" branch main shared
G_SEED="$(capture --dir "$G" --cap "$G_ROOT" session open --from=shared | tr -d '\n')"
run --dir "$G" --cap "$G_ROOT" mount --ns "$G_SEED" / ref:shared --rw
run --dir "$G" --cap "$G_ROOT" write --ns "$G_SEED" /seed.txt --text v0
run --dir "$G" --cap "$G_ROOT" checkin --ns "$G_SEED" -m seed
G_WIN="$(capture --dir "$G" --cap "$G_ROOT" session open --from=shared | tr -d '\n')"
run --dir "$G" --cap "$G_ROOT" mount --ns "$G_WIN" / ref:shared --rw
G_LOSE="$(capture --dir "$G" --cap "$G_ROOT" session open --from=shared | tr -d '\n')"
run --dir "$G" --cap "$G_ROOT" mount --ns "$G_LOSE" / ref:shared --rw
run --dir "$G" --cap "$G_ROOT" write --ns "$G_WIN" /w.txt --text w
run --dir "$G" --cap "$G_ROOT" write --ns "$G_LOSE" /l.txt --text l
run --dir "$G" --cap "$G_ROOT" checkin --ns "$G_WIN" -m w
# "forked <requested> -> <fork> ours=<oid> theirs=<oid>"
G_FORKLINE="$(capture --dir "$G" --cap "$G_ROOT" checkin --ns "$G_LOSE" -m l | tr -d '\r')"
G_FORK="$(printf '%s\n' "$G_FORKLINE" | awk '/^forked /{print $4; exit}')"
# #343: a SESSION fork lands inside the losing agent's own capability scope,
# because I18 retargets that session's mount at it. A merge or import fork,
# which retargets no session, still lands under the flat forks/ tree.
case "$G_FORK" in
heads/agents/*/forks/shared/*) ;;
*) die "expected the losing checkin to fork, got: $G_FORKLINE" ;;
esac
# A session that wrote but never checked in: staged work abandon must protect.
G_STAGED="$(capture --dir "$G" --cap "$G_ROOT" session open --from=shared | tr -d '\n')"
run --dir "$G" --cap "$G_ROOT" mount --ns "$G_STAGED" / ref:shared --rw
run --dir "$G" --cap "$G_ROOT" write --ns "$G_STAGED" /staged.txt --text staged

ZERO_OID="$(printf '0%.0s' $(seq 1 64))"

echo "# CLI_ABI.md conformance, binary: $FORGE"
echo

# --- exit 0: success -------------------------------------------------------
check abi/0-refs blocking 0 "" -- --dir "$A" --cap "$A_ROOT" refs
check abi/0-fsck-full blocking 0 "" -- --dir "$A" --cap "$A_ROOT" fsck --full
check abi/0-fsck-json blocking 0 "" -- --dir "$A" --cap "$A_ROOT" fsck --full --json
check abi/0-verify-sealed-tag blocking 0 "" -- --dir "$A" --cap "$A_ROOT" verify abi-seal
check abi/0-show-sealed-tag blocking 0 "" -- --dir "$A" --cap "$A_ROOT" show tags/abi-seal
# I25: a receipt reports only what it could verify from durable bytes.
check abi/0-receipt-show blocking 0 \
	"every object the receipt names is reread, rehashed and type-checked (I25)" -- \
	--dir "$A" --cap "$A_ROOT" receipt show "heads/agents/anon/$A_NS"
check abi/0-mv-blob blocking 0 \
	"a move is staged atomically and publishes through the ordinary checkin (I24)" -- \
	--dir "$A" --cap "$A_ROOT" mv --ns "$A_MVNS" /abi.txt /abi-moved.txt

# --- exit 1: denied / capability / input / not-found ----------------------
# I14: no ambient root authority.
check abi/1-no-capability blocking 1 "" -- --dir "$A" refs
check abi/1-malformed-cap-token blocking 1 "" -- --dir "$A" --cap not-a-real-token refs
check abi/1-unknown-tag blocking 1 "" -- --dir "$A" --cap "$A_ROOT" verify no-such-tag
check abi/1-unknown-path blocking 1 "" -- --dir "$A" --cap "$A_ROOT" read --ns "$A_NS" /main/absent.txt
check abi/1-write-without-payload blocking 1 "" -- --dir "$A" --cap "$A_ROOT" write --ns "$A_NS" /nope.txt
check abi/1-malformed-oid-spec blocking 1 "" -- --dir "$A" --cap "$A_ROOT" show oid:not-a-hex-object-id
check abi/1-bad-tag-charset blocking 1 "" -- --dir "$A" --cap "$A_INT" seal main --tag 'bad!tag'
check abi/1-no-repository-here blocking 1 "" -- --dir "$E" --cap "$A_ROOT" refs
# I24: a move that cannot be made atomically is refused, never half-applied.
check abi/1-mv-absent-source blocking 1 "" -- \
	--dir "$A" --cap "$A_ROOT" mv --ns "$A_MVNS" /absent.txt /moved.txt
check abi/1-mv-mount-root blocking 1 "" -- \
	--dir "$A" --cap "$A_ROOT" mv --ns "$A_MVNS" / /moved
check abi/1-mv-same-path blocking 1 "" -- \
	--dir "$A" --cap "$A_ROOT" mv --ns "$A_MVNS" /abi-moved.txt /abi-moved.txt
# Raw tree resolution stays rejected at the API boundary, not silently applied.
check abi/1-raw-merge-resolution blocking 1 "" -- \
	--dir "$A" --cap "$A_INT" merge --into=main --from "heads/agents/anon/$A_NS" --resolved "$ZERO_OID"
# I5/I13: a write+branch scoped agent cap may not advance a protected ref, and
# an attenuated cap may not run whole-repository fsck.
check abi/1-attenuated-cap-cannot-merge blocking 1 "" -- \
	--dir "$A" --cap "$A_ALICE" merge --into=main --from "heads/agents/anon/$A_NS"
check abi/1-attenuated-cap-cannot-fsck blocking 1 "" -- --dir "$A" --cap "$A_ALICE" fsck --full

# --- #12 / #309: abandon and gc ------------------------------------------
# Ordered: every refusal is exercised before the row that actually retires.
check abi/1-abandon-non-fork-ref blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork main
check abi/1-abandon-missing-fork blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork forks/shared/anon/01ARZ3NDEKTSV4RRFFQ69G5FAV
check abi/1-abandon-missing-session-fork blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork heads/agents/anon/forks/shared/01ARZ3NDEKTSV4RRFFQ69G5FAV
# A live session head shares the heads/ prefix with every session fork and is
# still not a fork: abandon must keep refusing it (I18).
check abi/1-abandon-live-session-head blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork "heads/agents/anon/$G_STAGED"
check abi/1-abandon-mounted-fork blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork "$G_FORK"
check abi/1-abandon-session-with-staged-work blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon session "$G_STAGED"
check abi/0-abandon-session blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon session "$G_LOSE"
check abi/0-abandon-fork blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork "$G_FORK"
check abi/1-abandon-fork-twice blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" abandon fork "$G_FORK"
# A retired name is retired for good; recreating it would corrupt the reflog chain.
check abi/1-recreate-retired-fork blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" branch main "$G_FORK"
# The tombstone must not read as catalog corruption.
check abi/0-fsck-full-after-abandon blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" fsck --full
check abi/0-gc-dry-run blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" gc --dry-run
check abi/0-gc-dry-run-json blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" gc --dry-run --min-age-secs 0 --json
# Collection is not implemented, so omitting --dry-run is an input error.
check abi/1-gc-neither-flag blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" gc

check abi/1-gc-both-flags blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" gc --dry-run --collect

# The floor is the only bound on the put-to-publish window, so it is refused
# rather than quietly raised.
check abi/1-gc-collect-under-the-floor blocking 1 "" -- \
	--dir "$G" --cap "$G_ROOT" gc --collect --min-age-secs 1

check abi/0-gc-collect blocking 0 "" -- \
	--dir "$G" --cap "$G_ROOT" gc --collect
# A filtered ref view is not a root set (I13/I14).
check abi/1-gc-attenuated-cap blocking 1 "" -- \
	--dir "$A" --cap "$A_ALICE" gc --dry-run

# --- exit 2: corruption or sealed-state violation -------------------------
check abi/2-bitrot-fails-closed blocking 2 "" -- --dir "$D" --cap "$D_ROOT" fsck --full

# Issue #348: `fsck --full` on an intact repository whose catalog is still at an
# older metadata schema version is exit 1, not exit 2. Age is not damage, and
# exit 2 is reserved for corruption -- the row above is what corruption looks
# like. Declared rather than faked, per this script's own rule: building an
# un-migrated catalog needs either a previous release's binary or a SQL client,
# and this harness deliberately requires neither (issue #346).
declare_unexercised abi/1-fsck-unmigrated-catalog 1 \
	"needs a catalog at an older metadata schema version; exercised against this same binary by an_unmigrated_catalog_is_an_input_error_not_corruption in crates/forge-cli/tests/cli_abi_exit_codes.rs - see #348"

# --- exit 3: transient busy / contention ----------------------------------
declare_unexercised abi/3-busy 3 \
	"needs a second process holding a SQLite write txn past busy_timeout; not deterministic in a single-process script - see #147 / docs/BENCH.md W5"

# --- exit 4: stale observation or merge conflict --------------------------
check abi/4-merge-conflict blocking 4 "" -- \
	--dir "$B" --cap "$B_INT" merge --into=main --from "heads/agents/anon/$B_B"
check abi/4-stale-observation blocking 4 "" -- \
	--dir "$C" --cap "$C_BOB" checkin --ns "$C_BOBNS" -m 'stale notes'
# I24: --expect-oid is an assumption about the source, and a wrong one is the
# same stale-observation row -- the caller described a state that is not there.
check abi/4-mv-expect-oid-mismatch blocking 4 "" -- \
	--dir "$A" --cap "$A_ROOT" mv --ns "$A_MVNS" /abi-moved.txt /moved.txt --expect-oid "$ZERO_OID"
# A seal is a claim about a ref, so it CASes the ref it names and a head that
# moved inside the seal window is the same stale observation (#331). The race
# itself needs the debug-only FORGEFS_TEST_SEAL_CAS_BARRIER seam, which a
# release binary does not contain, so the row is declared rather than faked.
declare_unexercised abi/4-seal-moved-head 4 \
	"needs a second process to move the ref between seal's read and its publish; driven deterministically by crates/forge-cli/tests/cli_seal_head_moves.rs via FORGEFS_TEST_SEAL_CAS_BARRIER, which exists only in debug builds"
check abi/0-seal-attest blocking 0 \
	"seal --attest re-reads the durable bytes before reporting success (I15)" -- \
	--dir "$A" --cap "$A_INT" seal main --tag abi-seal-attested --attest
check abi/1-seal-unknown-ref blocking 1 \
	"a ref the caller named that does not exist is not-found, never an internal failure" -- \
	--dir "$A" --cap "$A_INT" seal heads/definitely-absent --tag abi-seal-absent

# --- exit 5: I/O, SQLite, or internal failure -----------------------------
# Contract: exit 5 is for genuine I/O, SQLite or internal failure. It must NOT
# be reachable from caller-controlled input. These rows enforce that rule, so a
# regression that reintroduces an exit 5 (or a silent exit 0) from ordinary
# caller input fails the release.
check abi/1-duplicate-branch-name blocking 1 \
	"a caller-supplied duplicate ref name is an input error; insert_ref names the condition instead of leaking the refs PRIMARY KEY" -- \
	--dir "$A" --cap "$A_ROOT" branch main heads/abi-dup
check abi/2-duplicate-seal-tag blocking 2 \
	"a frozen tag is a sealed-state violation (INVARIANTS I5/I7); commit_seal returns Error::Sealed instead of a PRIMARY KEY violation" -- \
	--dir "$A" --cap "$A_INT" seal main --tag abi-seal
check abi/1-non-utf8-cap-file blocking 1 \
	"malformed caller input; load_cap reads bytes and maps non-UTF-8 to Error::Cap, matching forge-cap" -- \
	--dir "$A" --cap "$A_NONUTF8" refs
check abi/1-write-file-missing blocking 1 \
	"a --file path the caller got wrong is a bad argument, validated before the read" -- \
	--dir "$F" --cap "$F_ROOT" write --ns "$F_NS" /p.txt --file "$F_MISSING_FILE"
check abi/1-import-not-a-directory blocking 1 \
	"wrong argument kind, validated before the import (needs the import arg-id rename, otherwise open() fails with ENOTDIR first)" -- \
	--dir "$F" --cap "$F_ROOT" import "$F_PLAIN_FILE" --ref heads/imported-abi

# clap used to exit the process itself with its own default code 2, which
# CLI_ABI.md defines as "corruption or sealed-state violation" - so a mistyped
# subcommand was indistinguishable from a corrupt repository, the most
# dangerous confusion in the table. main() now parses explicitly and maps
# clap's ErrorKind, leaving 2 to mean only what the contract says.
check abi/1-unknown-subcommand blocking 1 \
	"clap usage errors are input errors; main() parses with try_parse and maps ErrorKind, so 2 stays reserved for corruption" -- \
	--dir "$F" --cap "$F_ROOT" no-such-subcommand
check abi/1-unknown-flag blocking 1 \
	"clap usage errors are input errors; see abi/1-unknown-subcommand" -- \
	--dir "$F" --cap "$F_ROOT" refs --bogus-flag

# Absent things. A silent exit 0 is worse than a wrong non-zero code, because
# automation keyed on exit codes cannot see it at all.
check abi/1-log-unknown-ref blocking 1 \
	"not-found is exit 1; log used to exit 0 with no output, hiding the difference between no history and no such ref" -- \
	--dir "$F" --cap "$F_ROOT" log no/such/ref
# I10: a commit with no receipt is absence, not corruption -- exit 1, not 2.
check abi/1-receipt-of-a-commit-without-one blocking 1 \
	"a merge or canonical commit legitimately carries no Contribution (I10)" -- \
	--dir "$A" --cap "$A_ROOT" receipt show main
# An object the CALLER named that is not here is a not-found input error. An
# object a RECEIPT names that is not here is corruption (exit 2). Collapsing
# the two would report a typo as a damaged repository.
check abi/1-receipt-of-an-absent-object blocking 1 \
	"naming an object this repository does not hold is input, not corruption" -- \
	--dir "$A" --cap "$A_ROOT" receipt show "oid:$ZERO_OID"
check abi/1-landmark-absent-oid blocking 1 \
	"not-found is exit 1; landmark verifies the object exists and records its real type instead of hardcoding 'commit'" -- \
	--dir "$F" --cap "$F_ROOT" landmark "$ZERO_OID"
# Keep this row last: it leaves the fixture in a state fsck calls corruption.
check abi/1-mount-unknown-ref blocking 1 \
	"not-found is exit 1; mount resolves its spec before persisting, so a dangling mount can no longer make fsck --full report corruption on intact bytes" -- \
	--dir "$F" --cap "$F_ROOT" mount --ns "$F_NS" /dangling no-such-ref-at-all

echo

ABI_OUT="$OUTDIR/abi-conformance.json"
if [ "$blocking_fail" -eq 0 ]; then
	abi_ok=true
else
	abi_ok=false
fi

{
	printf '{\n'
	json_field 2 binary "$FORGE" ,
	json_raw_field 2 blocking_failures "$blocking_fail" ,
	json_field 2 contract CLI_ABI.md ,
	json_field 2 known_failing_reference \
		"https://github.com/zozo123/forgefs/issues/237" ,
	json_raw_field 2 ok "$abi_ok" ,
	printf '  "rows": [\n'
	if [ -s "$ROWS_JSON" ]; then
		cat "$ROWS_JSON"
		printf '\n'
	fi
	printf '  ],\n'
	json_raw_field 2 rows_blocking "$rows_blocking" ,
	json_raw_field 2 rows_known_failing "$rows_known_failing" ,
	json_raw_field 2 rows_total "$row_count" ,
	json_raw_field 2 rows_unexercised "$rows_unexercised"
	printf '}\n'
} >"$ABI_OUT"

# scripts/release-gate.sh reprints these numbers inside its own summary. It
# asks for them here, as the ready-made `cli_abi` block, so that neither script
# ever has to read the other's JSON back in. The path is deliberately the
# caller's and not $OUTDIR: this is a channel between two scripts, not a
# release artifact, and a direct run leaves the variable unset and writes
# nothing.
if [ -n "${ABI_GATE_FRAGMENT:-}" ]; then
	{
		printf '{\n'
		json_raw_field 4 blocking_failures "$blocking_fail" ,
		printf '    "known_failing": [\n'
		if [ -s "$KNOWN_FAILING_JSON" ]; then
			cat "$KNOWN_FAILING_JSON"
			printf '\n'
		fi
		printf '    ],\n'
		json_raw_field 4 ok "$abi_ok" ,
		json_raw_field 4 rows_blocking "$rows_blocking" ,
		json_raw_field 4 rows_known_failing "$rows_known_failing" ,
		json_raw_field 4 rows_total "$row_count" ,
		json_raw_field 4 rows_unexercised "$rows_unexercised"
		printf '  }\n'
	} >"$ABI_GATE_FRAGMENT"
fi

printf 'abi rows=%d blocking=%d known_failing=%d unexercised=%d blocking_failures=%d\n' \
	"$row_count" "$rows_blocking" "$rows_known_failing" "$rows_unexercised" \
	"$blocking_fail"

echo "abi report: $ABI_OUT"

if [ "$blocking_fail" -ne 0 ]; then
	printf 'abi-conformance: %d blocking row(s) violate CLI_ABI.md\n' "$blocking_fail" >&2
	exit 1
fi
echo "abi-conformance: CLI_ABI.md contract holds for every blocking row"
