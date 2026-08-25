#!/usr/bin/env bash
# scripts/release-gate.sh
#
# The ForgeFS release gate: ForgeFS seals and verifies ITSELF before any bytes
# are published. Every assertion below is one of the repository's own stated
# contracts, exercised through the shipped `forge` binary as a real process.
#
# Usage:
#   scripts/release-gate.sh <path-to-forge-binary> [OUTDIR]
#
# One argument is enough. A human can run exactly what CI runs:
#   cargo build --release --locked -p forge-cli
#   scripts/release-gate.sh target/release/forge
#
# OUTDIR defaults to ./release-gate-out and always receives, pass or fail:
#   gate-summary.json      machine-readable result: phases, failures, versions
#   fsck-full.json         `forge fsck --full --json` report, verbatim
#   seal-attestation.txt   sealed tag, snapshot OID, verify result, ref flags
#   abi-conformance.json   CLI_ABI.md exit-code table (see cli-abi-conformance.sh)
#   env-line.txt           docs/BENCH.md required environment line
#   env-line.json          the same environment line, machine-readable
#   conflict-object.txt    the Conflict object produced by the overlap phase
#
# Exit status:
#   0  every gate assertion held
#   1  at least one gate assertion failed (see gate-summary.json)
#   2  harness failure: bad usage, or a missing/unusable forge binary
#   3  a prerequisite this harness needs is missing; the message names it
#
# Prerequisites, in full: bash, coreutils, sed, awk, and the `forge` binary
# under test. That is the whole list, and it is the point: this gate is what a
# cautious user runs before trusting a release, so it must have FEWER
# prerequisites than the product it verifies, not more. It used to demand
# `python3` for JSON shaping alone and refuse to start without it with exit 2 --
# the code CLI_ABI.md reserves for corruption -- on any machine without it, a
# base Debian image included (issue #346). JSON is shaped by
# scripts/json-lib.sh now, and a missing prerequisite has an exit code of its
# own.
#
# That list is now checked rather than merely claimed. It was false: `grep` is
# its own Debian package, not part of coreutils, and one `grep -Eq` here
# reported a missing TOOL as a failing PRODUCT -- exit 1, and `"ok": false` in
# gate-summary.json against a repository nothing was wrong with (issue #354).
# The match it did is done in awk now, and scripts/prereq-lib.sh checks every
# declared command before the first assertion runs, so an absent prerequisite
# exits 3 naming itself, while an UNDECLARED command that this script reaches
# for anyway is caught at the point of use and turned into the same exit 3.
#
# Environment:
#   FORGE_GATE_TAG   seal tag to use (default: v<forge --version>)
#   FORGE_ENV_COMMIT commit sha recorded in the environment line
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

FORGE="${1:-}"
OUTDIR="${2:-./release-gate-out}"

harness_die() {
	printf 'release-gate: harness error: %s\n' "$1" >&2
	exit 2
}

[ -n "$FORGE" ] || harness_die "usage: $0 <path-to-forge-binary> [OUTDIR]"
[ -x "$FORGE" ] || harness_die "not an executable forge binary: $FORGE"
[ -r "$SCRIPT_DIR/prereq-lib.sh" ] || {
	printf 'release-gate: missing %s\n' "$SCRIPT_DIR/prereq-lib.sh" >&2
	exit 3
}
PREREQ_SCRIPT=release-gate
# shellcheck source=scripts/prereq-lib.sh
. "$SCRIPT_DIR/prereq-lib.sh"
# Before anything else runs: a tool this gate needs and this machine lacks is a
# harness error with an exit code of its own, never a gate assertion that
# failed (issue #354).
require_declared_commands
[ -r "$SCRIPT_DIR/json-lib.sh" ] || {
	printf 'release-gate: missing %s\n' "$SCRIPT_DIR/json-lib.sh" >&2
	exit 3
}
# shellcheck source=scripts/json-lib.sh
. "$SCRIPT_DIR/json-lib.sh"
[ -x "$SCRIPT_DIR/cli-abi-conformance.sh" ] ||
	harness_die "missing $SCRIPT_DIR/cli-abi-conformance.sh"
[ -x "$SCRIPT_DIR/forge-env-line.sh" ] ||
	harness_die "missing $SCRIPT_DIR/forge-env-line.sh"

FORGE="$(cd "$(dirname "$FORGE")" && pwd)/$(basename "$FORGE")"
mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"

# No ambient authority (I14) and no inherited repository may leak in.
unset FORGE_CAP FORGE_DIR

FORGE_VERSION="$("$FORGE" --version 2>/dev/null | awk '{print $2}')"
[ -n "$FORGE_VERSION" ] || harness_die "could not read 'forge --version'"
TAG="${FORGE_GATE_TAG:-v$FORGE_VERSION}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/forge-release-gate.XXXXXX")"
DEMO="$WORK/forge"
PHASES="$WORK/phases"
FAILURES="$WORK/failures"
# From here on, a command bash cannot find is recorded here by
# scripts/prereq-lib.sh, and `fail` refuses to blame forge for it.
PREREQ_MARKER="$WORK/missing-commands"
: >"$PHASES"
: >"$FAILURES"
US=$'\x1f'
START_EPOCH="$(date +%s)"

# scripts/cli-abi-conformance.sh writes the summary's `cli_abi` block here,
# outside OUTDIR: it is a channel between the two scripts, not a release
# artifact, and it is what keeps this gate from having to read the conformance
# report's JSON back in.
export ABI_GATE_FRAGMENT="$WORK/cli-abi-fragment.json"

# Capability tokens ARE authority (I13/I14) and must never reach a log line or
# a CI artifact, not even a throwaway fixture's token.
redact() {
	sed -e 's/fmac1_[0-9a-fA-F]*/fmac1_<redacted-capability-token>/g'
}

note() { printf '%s\n' "$*"; }

# ere_match <extended-regex> <text> - true when any LINE of <text> matches.
#
# What `grep -Eq` did, in the awk this script already depends on. `grep` is a
# package of its own and was never a declared prerequisite (issue #354). The
# pattern travels in the environment rather than through `awk -v`, because -v
# processes escape sequences in the value and would eat the backslash out of
# `\.` before the regex engine ever saw it.
ere_match() {
	printf '%s\n' "$2" |
		GATE_ERE="$1" awk '$0 ~ ENVIRON["GATE_ERE"] { hit = 1 } END { exit hit ? 0 : 1 }'
}

phase() {
	printf '%s%s%s\n' "$1" "$US" "$(printf '%s' "$2" | redact | tr '\n' ' ')" >>"$PHASES"
	printf 'gate: ok    %-38s %s\n' "$1" "$(printf '%s' "$2" | redact | tr '\n' ' ')"
}

fail() {
	local id="$1" detail="$2"
	# The single funnel for "forge did not honour a contract", and therefore
	# the single place that can be lied to by a missing tool. If bash could
	# not find a command during this run, whatever is about to be blamed on
	# forge is this harness's fault; exit 3 instead (issue #354).
	prereq_guard
	printf '%s%s%s\n' "$id" "$US" "$(printf '%s' "$detail" | redact | tr '\n' ' ')" >>"$FAILURES"
	printf 'gate: FAIL  %-38s %s\n' "$id" "$(printf '%s' "$detail" | redact | tr '\n' ' ')" >&2
}

# Everything the gate summary needs about phase 7, filled in there. Declared
# here because `finish` runs as an EXIT trap and must be able to say "the fsck
# phase never ran" -- which is what an empty FSCK_OK means -- for a gate that
# died earlier.
FSCK_OK=""
FSCK_FULL=""
FSCK_REFS=""
FSCK_OBJECTS=""
FSCK_NAMESPACES=""
FSCK_FINDINGS=""

# One JSON array of {detail, id} objects from a US-separated record file.
gate_record_array() {
	local file="$1" indent="$2" first=1 line id detail
	if [ ! -s "$file" ]; then
		printf '[]'
		return 0
	fi
	printf '[\n'
	while IFS= read -r line; do
		[ -n "$line" ] || continue
		id="${line%%"$US"*}"
		detail="${line#*"$US"}"
		[ "$first" -eq 1 ] || printf ',\n'
		first=0
		printf '%*s{\n' "$((indent + 2))" ""
		json_field "$((indent + 4))" detail "$detail" ,
		json_field "$((indent + 4))" id "$id"
		printf '%*s}' "$((indent + 2))" ""
	done <"$file"
	printf '\n%*s]' "$indent" ""
}

# The list of artifacts this run was supposed to leave behind, each marked
# present or not. Deliberately reported for a failed gate too: which artifacts
# exist is itself the first diagnostic.
gate_artifact_array() {
	local indent="$1" first=1 name present
	shift
	printf '[\n'
	for name in "$@"; do
		if [ -e "$OUTDIR/$name" ]; then present=true; else present=false; fi
		[ "$first" -eq 1 ] || printf ',\n'
		first=0
		printf '%*s{\n' "$((indent + 2))" ""
		json_field "$((indent + 4))" file "$name" ,
		json_raw_field "$((indent + 4))" present "$present"
		printf '%*s}' "$((indent + 2))" ""
	done
	printf '\n%*s]' "$indent" ""
}

# Always emit the summary, including on failure, so the artifact is diagnosable.
finish() {
	local status=$?
	local failed ok fsck_block cli_abi_block environment_block
	# A gate that could not run produced no verdict, so it must not leave a
	# document that reads like one. `"ok": false` beside a missing-tool
	# message is exactly the fabricated product failure issue #354 is about,
	# and a stale summary from an earlier run would be read the same way.
	if prereq_missing; then
		rm -f "$OUTDIR/gate-summary.json" 2>/dev/null || true
		printf 'release-gate: no gate-summary.json written: the harness could not run\n' >&2
		rm -rf "$WORK" 2>/dev/null || true
		exit 3
	fi
	failed="$(wc -l <"$FAILURES" | tr -d ' ')"

	if [ "$failed" -eq 0 ] && [ "$status" -eq 0 ]; then ok=true; else ok=false; fi

	# Phase 7 hands these over as it validates them, rather than the summary
	# reading fsck-full.json back: the values are already in this shell, and
	# re-reading a file to learn what this process just decided is how a
	# summary drifts from the check it claims to report.
	if [ -n "$FSCK_OK" ]; then
		fsck_block="$(
			printf '{\n'
			json_raw_field 6 checked_namespaces "$FSCK_NAMESPACES" ,
			json_raw_field 6 checked_objects "$FSCK_OBJECTS" ,
			json_raw_field 6 checked_refs "$FSCK_REFS" ,
			json_raw_field 6 findings "$FSCK_FINDINGS" ,
			json_raw_field 6 full "$FSCK_FULL" ,
			json_raw_field 6 ok "$FSCK_OK"
			printf '    }'
		)"
	else
		fsck_block=null
	fi

	if [ -s "$ABI_GATE_FRAGMENT" ]; then
		cli_abi_block="$(cat "$ABI_GATE_FRAGMENT")"
	else
		cli_abi_block=null
	fi

	# env-line.json is already a JSON object; it is spliced whole rather than
	# taken apart and put back together.
	if [ -s "$OUTDIR/env-line.json" ]; then
		environment_block="$(sed -e '2,$s/^/  /' "$OUTDIR/env-line.json")"
	else
		environment_block=null
	fi

	{
		printf '{\n'
		printf '  "artifacts": '
		gate_artifact_array 2 \
			fsck-full.json \
			seal-attestation.txt \
			abi-conformance.json \
			env-line.txt \
			env-line.json \
			conflict-object.txt
		printf ',\n'
		printf '  "cli_abi": %s,\n' "$cli_abi_block"
		json_raw_field 2 duration_seconds "$(($(date +%s) - START_EPOCH))" ,
		printf '  "environment": %s,\n' "$environment_block"
		printf '  "failures": '
		gate_record_array "$FAILURES" 2
		printf ',\n'
		json_field 2 forge_binary "$FORGE" ,
		json_field 2 forge_version "$FORGE_VERSION" ,
		printf '  "fsck": %s,\n' "$fsck_block"
		json_raw_field 2 ok "$ok" ,
		printf '  "phases_passed": '
		gate_record_array "$PHASES" 2
		printf ',\n'
		json_field 2 schema forgefs.release-gate/1 ,
		json_raw_field 2 script_exit_status "$status" ,
		json_field 2 seal_tag "$TAG" ,
		json_raw_field 2 started_unix "$START_EPOCH"
		printf '}\n'
	} >"$OUTDIR/gate-summary.json" || true

	rm -rf "$WORK"
	if [ "$status" -eq 0 ] && [ "$failed" -ne 0 ]; then
		printf 'release-gate: %s assertion(s) failed\n' "$failed" >&2
		exit 1
	fi
	exit "$status"
}
trap finish EXIT

# ---------------------------------------------------------------------------
# forge invocation helpers
# ---------------------------------------------------------------------------
LAST_OUT=""
LAST_STATUS=0

# forge_run <argv...>  - capture stdout+stderr and the exit status, never abort
forge_run() {
	set +e
	LAST_OUT="$("$FORGE" "$@" 2>&1)"
	LAST_STATUS=$?
	set -e
}

# must <label> <argv...> - the command has to succeed for the gate to mean
# anything; a failure here is a harness/regression stop, reported as a failure.
must() {
	local label="$1"
	shift
	forge_run "$@"
	if [ "$LAST_STATUS" -ne 0 ]; then
		fail "$label" "expected success, got exit $LAST_STATUS: $LAST_OUT"
		exit 1
	fi
}

# expect_exit <label> <expected> <argv...>
expect_exit() {
	local label="$1" expected="$2"
	shift 2
	forge_run "$@"
	if [ "$LAST_STATUS" -ne "$expected" ]; then
		fail "$label" "expected exit $expected, got $LAST_STATUS: $LAST_OUT"
		exit 1
	fi
}

ref_oid() {
	# `forge refs` prints: <flags> <kind padded> <name> <oid>
	"$FORGE" --dir "$DEMO" --cap "$ROOT" refs 2>/dev/null |
		awk -v n="$1" '$3 == n {print $4}'
}

ref_flags() {
	"$FORGE" --dir "$DEMO" --cap "$ROOT" refs 2>/dev/null |
		awk -v n="$1" '$3 == n {print $1}'
}

note "release-gate: forge $FORGE_VERSION at $FORGE"
note "release-gate: seal tag $TAG"
note "release-gate: workdir $WORK"
note ""

# ---------------------------------------------------------------------------
# Phase 1 - init a fresh forge
# ---------------------------------------------------------------------------
mkdir -p "$DEMO"
must gate/init init "$DEMO"
ROOT="$DEMO/.forge/keys/root.cap"
INT="$DEMO/.forge/keys/integrator.cap"
for f in "$ROOT" "$INT" "$DEMO/.forge/VERSION"; do
	[ -f "$f" ] || {
		fail gate/init "init did not produce $f"
		exit 1
	}
done
VERSION_BYTES="$(tr -d '\n' <"$DEMO/.forge/VERSION")"
if [ "$VERSION_BYTES" != "1" ]; then
	fail gate/repo-version "expected repository VERSION 1, got '$VERSION_BYTES'"
	exit 1
fi
phase gate/init "fresh forge at $DEMO, repository VERSION $VERSION_BYTES"

# ---------------------------------------------------------------------------
# Phase 2 - grant two agent capabilities (plus a control agent)
# ---------------------------------------------------------------------------
grant_agent() {
	local agent="$1"
	must "gate/grant-$agent" --dir "$DEMO" --cap "$ROOT" grant \
		--ops read,write,branch --ref "main,heads/agents/$agent/*" --agent "$agent"
	printf '%s' "$LAST_OUT" | tr -d '\n'
}
ALICE="$(grant_agent alice)"
BOB="$(grant_agent bob)"
CAROL="$(grant_agent carol)"
for tok in "$ALICE" "$BOB" "$CAROL"; do
	case "$tok" in
	fmac1_*) ;;
	*)
		fail gate/grant "grant did not return a capability token"
		exit 1
		;;
	esac
done
phase gate/grant "three attenuated agent caps minted (alice, bob, carol)"

# ---------------------------------------------------------------------------
# Phase 3 - the README two-agent path
#   two sessions, disjoint writes, two checkins, two merges
# ---------------------------------------------------------------------------
must gate/session-alice --dir "$DEMO" --cap "$ALICE" session open --from=main
A_NS="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/session-bob --dir "$DEMO" --cap "$BOB" session open --from=main
B_NS="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
[ -n "$A_NS" ] && [ -n "$B_NS" ] || {
	fail gate/two-agent "session open returned no namespace id"
	exit 1
}

must gate/write-alice --dir "$DEMO" --cap "$ALICE" write --ns "$A_NS" /a.txt --text alice
must gate/write-bob --dir "$DEMO" --cap "$BOB" write --ns "$B_NS" /b.txt --text bob

must gate/checkin-alice --dir "$DEMO" --cap "$ALICE" checkin --ns "$A_NS" -m alice
case "$LAST_OUT" in *updated*) ;; *)
	fail gate/checkin-alice "expected 'updated', got: $LAST_OUT"
	exit 1
	;;
esac
must gate/checkin-bob --dir "$DEMO" --cap "$BOB" checkin --ns "$B_NS" -m bob
case "$LAST_OUT" in *updated*) ;; *)
	fail gate/checkin-bob "expected 'updated', got: $LAST_OUT"
	exit 1
	;;
esac

MAIN_BEFORE="$(ref_oid main)"
must gate/merge-alice --dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/alice/$A_NS"
must gate/merge-bob --dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/bob/$B_NS"
MAIN_AFTER="$(ref_oid main)"
if [ -z "$MAIN_AFTER" ] || [ "$MAIN_AFTER" = "$MAIN_BEFORE" ]; then
	fail gate/two-agent "main did not advance across two disjoint merges ($MAIN_BEFORE -> $MAIN_AFTER)"
	exit 1
fi
# Both disjoint contributions must be readable from main in a fresh session.
must gate/read-back --dir "$DEMO" --cap "$ROOT" session open --from=main
READBACK_NS="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/read-a --dir "$DEMO" --cap "$ROOT" read --ns "$READBACK_NS" /main/a.txt
[ "$LAST_OUT" = "alice" ] || {
	fail gate/read-a "expected 'alice', got: $LAST_OUT"
	exit 1
}
must gate/read-b --dir "$DEMO" --cap "$ROOT" read --ns "$READBACK_NS" /main/b.txt
[ "$LAST_OUT" = "bob" ] || {
	fail gate/read-b "expected 'bob', got: $LAST_OUT"
	exit 1
}
phase gate/two-agent-path "2 sessions, disjoint writes, 2 checkins, 2 merges; main $MAIN_BEFORE -> $MAIN_AFTER"

# ---------------------------------------------------------------------------
# Phase 4 - deliberate same-path overlap
#   I11: overlap is a Conflict object. docs/BENCH.md W4: exit 4, both immutable
#   inputs preserved, and a typed conflicts/ ref (I7).
# ---------------------------------------------------------------------------
must gate/overlap-session-a --dir "$DEMO" --cap "$ALICE" session open --from=main
OV_A="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/overlap-session-b --dir "$DEMO" --cap "$BOB" session open --from=main
OV_B="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/overlap-write-a --dir "$DEMO" --cap "$ALICE" write --ns "$OV_A" /overlap.txt --text ours
must gate/overlap-write-b --dir "$DEMO" --cap "$BOB" write --ns "$OV_B" /overlap.txt --text theirs
must gate/overlap-checkin-a --dir "$DEMO" --cap "$ALICE" checkin --ns "$OV_A" -m ours
must gate/overlap-checkin-b --dir "$DEMO" --cap "$BOB" checkin --ns "$OV_B" -m theirs
must gate/overlap-merge-a --dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/alice/$OV_A"

MAIN_PRE_CONFLICT="$(ref_oid main)"
expect_exit gate/overlap-merge-conflict 4 \
	--dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/bob/$OV_B"
CONFLICT_OID="$(printf '%s\n' "$LAST_OUT" | awk '/^conflict /{print $2; exit}')"
if [ -z "$CONFLICT_OID" ] || [ "${#CONFLICT_OID}" -ne 64 ]; then
	fail gate/overlap-merge-conflict "no machine-readable 'conflict <oid>' line: $LAST_OUT"
	exit 1
fi
# The conflict must be a real Conflict object, not a string, and it must
# preserve both immutable inputs.
must gate/conflict-object --dir "$DEMO" --cap "$ROOT" show "oid:$CONFLICT_OID"
printf '%s\n' "$LAST_OUT" >"$OUTDIR/conflict-object.txt"
CONFLICT_SHOW="$LAST_OUT"
# `show` renders conflict paths tree-relative, so /overlap.txt appears as
# `path overlap.txt`. Accept either spelling rather than pinning a cosmetic.
#
# No `{64}` in these patterns: mawk 1.3.4 -- what a Debian base image has, and
# the awk json-lib.sh is written against -- has no interval expressions, so
# `[0-9a-f]{64}` there matches the literal characters `{64}` and every side
# would be reported missing. The hex length is asserted below, on the captured
# value, which is a stronger check than the regex was anyway.
for want in "^conflict $CONFLICT_OID" "^ours [0-9a-f]+$" "^theirs [0-9a-f]+$" "^path /?overlap\.txt "; do
	if ! ere_match "$want" "$CONFLICT_SHOW"; then
		fail gate/conflict-object "Conflict object is missing /$want/: $CONFLICT_SHOW"
		exit 1
	fi
done
CONFLICT_OURS="$(printf '%s\n' "$CONFLICT_SHOW" | awk '/^ours /{print $2; exit}')"
CONFLICT_THEIRS="$(printf '%s\n' "$CONFLICT_SHOW" | awk '/^theirs /{print $2; exit}')"
if [ "${#CONFLICT_OURS}" -ne 64 ] || [ "${#CONFLICT_THEIRS}" -ne 64 ]; then
	fail gate/conflict-object \
		"conflict sides are not full object ids: ours=$CONFLICT_OURS theirs=$CONFLICT_THEIRS"
	exit 1
fi
if [ "$CONFLICT_OURS" = "$CONFLICT_THEIRS" ]; then
	fail gate/conflict-object "conflict ours == theirs; both sides were not preserved"
	exit 1
fi
# I7: the conflict is published under a typed conflicts/ ref, not a naming
# convention on heads/.
CONFLICT_REF="$("$FORGE" --dir "$DEMO" --cap "$ROOT" refs 2>/dev/null |
	awk -v o="$CONFLICT_OID" '$2 == "conflict" && $4 == o {print $3; exit}')"
if [ -z "$CONFLICT_REF" ]; then
	fail gate/conflict-object "no typed conflict ref names $CONFLICT_OID"
	exit 1
fi
case "$CONFLICT_REF" in
conflicts/main/*) ;;
*)
	fail gate/conflict-object "conflict ref is not under conflicts/main/: $CONFLICT_REF"
	exit 1
	;;
esac
# The losing merge must not have moved the destination ref.
if [ "$(ref_oid main)" != "$MAIN_PRE_CONFLICT" ]; then
	fail gate/overlap-merge-conflict "main advanced despite a merge conflict"
	exit 1
fi
phase gate/same-path-overlap "merge exit 4, Conflict $CONFLICT_OID at $CONFLICT_REF, main pinned at $MAIN_PRE_CONFLICT"

# ---------------------------------------------------------------------------
# Phase 5 - stale observation
#   I9: an observed path->oid that moved fails checkin even for disjoint writes.
#   docs/BENCH.md W3: exit 4 AND the destination ref does not advance.
# ---------------------------------------------------------------------------
must gate/stale-alice-v1-session --dir "$DEMO" --cap "$ALICE" session open --from=main
ST_A1="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/stale-alice-v1-write --dir "$DEMO" --cap "$ALICE" write --ns "$ST_A1" /doc.txt --text v1
must gate/stale-alice-v1-checkin --dir "$DEMO" --cap "$ALICE" checkin --ns "$ST_A1" -m v1
must gate/stale-alice-v1-merge --dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/alice/$ST_A1"

must gate/stale-bob-session --dir "$DEMO" --cap "$BOB" session open --from=main
ST_B="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/stale-bob-read --dir "$DEMO" --cap "$BOB" read --ns "$ST_B" /main/doc.txt
[ "$LAST_OUT" = "v1" ] || {
	fail gate/stale-bob-read "expected to observe 'v1', got: $LAST_OUT"
	exit 1
}

must gate/stale-alice-v2-session --dir "$DEMO" --cap "$ALICE" session open --from=main
ST_A2="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/stale-alice-v2-write --dir "$DEMO" --cap "$ALICE" write --ns "$ST_A2" /doc.txt --text v2
must gate/stale-alice-v2-checkin --dir "$DEMO" --cap "$ALICE" checkin --ns "$ST_A2" -m v2
must gate/stale-alice-v2-merge --dir "$DEMO" --cap "$INT" merge --into=main --from "heads/agents/alice/$ST_A2"

# Bob's write is disjoint from /doc.txt on purpose: only the stale observation
# may make this checkin fail.
must gate/stale-bob-write --dir "$DEMO" --cap "$BOB" write --ns "$ST_B" /notes.txt --text notes
STALE_DEST="heads/agents/bob/$ST_B"
DEST_BEFORE="$(ref_oid "$STALE_DEST")"
MAIN_BEFORE_STALE="$(ref_oid main)"
expect_exit gate/stale-checkin 4 --dir "$DEMO" --cap "$BOB" checkin --ns "$ST_B" -m 'stale notes'
case "$(printf '%s' "$LAST_OUT" | tr '[:upper:]' '[:lower:]')" in
*stale*) ;;
*)
	fail gate/stale-checkin "exit 4 without a stale diagnosis: $LAST_OUT"
	exit 1
	;;
esac
DEST_AFTER="$(ref_oid "$STALE_DEST")"
if [ "$DEST_AFTER" != "$DEST_BEFORE" ]; then
	fail gate/stale-checkin "destination ref $STALE_DEST advanced '$DEST_BEFORE' -> '$DEST_AFTER'"
	exit 1
fi
if [ "$(ref_oid main)" != "$MAIN_BEFORE_STALE" ]; then
	fail gate/stale-checkin "main advanced on a stale checkin"
	exit 1
fi
# The stale overlay must not have leaked into main either.
must gate/stale-fresh-session --dir "$DEMO" --cap "$BOB" session open --from=main
ST_FRESH="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
expect_exit gate/stale-no-leak 1 --dir "$DEMO" --cap "$BOB" read --ns "$ST_FRESH" /main/notes.txt

# Control: the same "did the ref advance?" probe must be able to SEE an
# advance, otherwise the assertion above proves nothing.
must gate/control-session --dir "$DEMO" --cap "$CAROL" session open --from=main
CTRL_NS="$(printf '%s' "$LAST_OUT" | tr -d '\n')"
must gate/control-write --dir "$DEMO" --cap "$CAROL" write --ns "$CTRL_NS" /control.txt --text independent
must gate/control-checkin --dir "$DEMO" --cap "$CAROL" checkin --ns "$CTRL_NS" -m control
CTRL_OID="$(ref_oid "heads/agents/carol/$CTRL_NS")"
if [ -z "$CTRL_OID" ]; then
	fail gate/control-checkin "control agent's ref did not appear; the no-advance probe is not trustworthy"
	exit 1
fi
phase gate/stale-observation "checkin exit 4, $STALE_DEST pinned at '${DEST_BEFORE:-<absent>}', control ref advanced to $CTRL_OID"

# ---------------------------------------------------------------------------
# Phase 6 - seal and verify
#   I15: verify rereads durable bytes and this forge's trusted seal key.
# ---------------------------------------------------------------------------
must gate/seal --dir "$DEMO" --cap "$INT" seal main --tag "$TAG" --attest
SEAL_OUT="$LAST_OUT"
SNAP_OID="$(printf '%s\n' "$SEAL_OUT" | awk -v t="tags/$TAG" '$1 == "sealed" && $2 == t {print $3; exit}')"
if [ -z "$SNAP_OID" ] || [ "${#SNAP_OID}" -ne 64 ]; then
	fail gate/seal "no 'sealed tags/$TAG <oid>' line: $SEAL_OUT"
	exit 1
fi
case "$SEAL_OUT" in *"attested ok"*) ;; *)
	fail gate/seal "--attest did not report 'attested ok': $SEAL_OUT"
	exit 1
	;;
esac
must gate/verify --dir "$DEMO" --cap "$ROOT" verify "$TAG"
VERIFY_OUT="$LAST_OUT"
VERIFY_OID="$(printf '%s\n' "$VERIFY_OUT" | awk '$1 == "ok" {print $2; exit}')"
if [ "$VERIFY_OID" != "$SNAP_OID" ]; then
	fail gate/verify "verify names $VERIFY_OID but seal published $SNAP_OID"
	exit 1
fi
# I5/I7: the tag ref is typed, protected and sealed. `forge refs` prints P/S.
TAG_FLAGS="$(ref_flags "tags/$TAG")"
if [ "$TAG_FLAGS" != "PS" ]; then
	fail gate/verify "tags/$TAG flags are '$TAG_FLAGS', expected 'PS' (protected+sealed)"
	exit 1
fi
{
	echo "forgefs seal attestation"
	echo "tag:              $TAG"
	echo "snapshot oid:     $SNAP_OID"
	echo "verify result:    $VERIFY_OUT"
	echo "tag ref flags:    $TAG_FLAGS (P=protected, S=sealed)"
	echo "forge version:    $FORGE_VERSION"
	echo "seal command:     forge --cap <integrator> seal main --tag $TAG --attest"
	echo "verify command:   forge --cap <root> verify $TAG"
	echo
	echo "-- seal output --"
	printf '%s\n' "$SEAL_OUT"
	echo "-- refs --"
	"$FORGE" --dir "$DEMO" --cap "$ROOT" refs 2>/dev/null | redact
} >"$OUTDIR/seal-attestation.txt"
phase gate/seal-verify "sealed tags/$TAG -> $SNAP_OID, --attest ok, verify ok, flags $TAG_FLAGS"

# ---------------------------------------------------------------------------
# Phase 7 - fsck --full --json, scanned as JSON (never grepped as prose)
# ---------------------------------------------------------------------------
must gate/fsck --dir "$DEMO" --cap "$ROOT" fsck --full --json
printf '%s\n' "$LAST_OUT" >"$OUTDIR/fsck-full.json"

# Scanned rather than pattern-matched: json_top_level tracks string state and
# bracket depth, so a brace, colon or comma inside a finding's own text cannot
# be mistaken for document structure.
fsck_members=""
fsck_problems=""
if ! fsck_members="$(json_top_level <"$OUTDIR/fsck-full.json")"; then
	fsck_problems=" fsck-full.json is not a JSON object;"
fi

fsck_value() { json_member "$fsck_members" "$1" || printf 'absent'; }

if [ -z "$fsck_problems" ]; then
	fsck_ok="$(fsck_value ok)"
	fsck_full="$(fsck_value full)"
	fsck_findings="$(fsck_value findings)"
	fsck_refs="$(fsck_value checked_refs)"
	fsck_objects="$(fsck_value checked_objects)"
	fsck_namespaces="$(fsck_value checked_namespaces)"

	[ "$fsck_ok" = true ] ||
		fsck_problems="$fsck_problems ok is $fsck_ok, expected true;"
	[ "$fsck_full" = true ] ||
		fsck_problems="$fsck_problems full is $fsck_full, expected true;"
	[ "$fsck_findings" = "[]" ] ||
		fsck_problems="$fsck_problems findings is non-empty: $fsck_findings;"
	for counted in "checked_refs=$fsck_refs" "checked_objects=$fsck_objects" \
		"checked_namespaces=$fsck_namespaces"; do
		case "${counted#*=}" in
		'' | 0 | *[!0-9]*)
			fsck_problems="$fsck_problems ${counted%%=*} is ${counted#*=}, expected a positive integer;"
			;;
		esac
	done
fi

if [ -n "$fsck_problems" ]; then
	fail gate/fsck "fsck --full --json report is not clean:$fsck_problems (see fsck-full.json)"
	exit 1
fi

# Only now do the summary's own fields exist: an unset FSCK_OK is what makes
# `finish` report "fsck": null for a gate that never got this far.
FSCK_OK="$fsck_ok"
FSCK_FULL="$fsck_full"
FSCK_FINDINGS="$fsck_findings"
FSCK_REFS="$fsck_refs"
FSCK_OBJECTS="$fsck_objects"
FSCK_NAMESPACES="$fsck_namespaces"
phase gate/fsck "fsck --full --json ok: refs=$FSCK_REFS objects=$FSCK_OBJECTS namespaces=$FSCK_NAMESPACES findings=0"

# ---------------------------------------------------------------------------
# Phase 8 - CLI exit-code ABI conformance (CLI_ABI.md, issue #237)
# ---------------------------------------------------------------------------
if "$SCRIPT_DIR/cli-abi-conformance.sh" "$FORGE" "$OUTDIR"; then
	phase gate/cli-abi "every blocking CLI_ABI.md row matched the contract"
else
	fail gate/cli-abi "CLI_ABI.md conformance failed (see abi-conformance.json)"
	exit 1
fi

# ---------------------------------------------------------------------------
# Phase 9 - record the docs/BENCH.md environment line for this release
# ---------------------------------------------------------------------------
export FORGE_ENV_COMMAND="forge --cap <integrator> seal main --tag $TAG --attest; forge fsck --full --json"
export FORGE_ENV_WORKERS="1 (correctness gate, not a throughput measurement)"
export FORGE_ENV_REPO_CLASS="$DEMO (fresh repository, created by this gate run)"
"$SCRIPT_DIR/forge-env-line.sh" --forge "$FORGE" "$DEMO" >"$OUTDIR/env-line.txt"
"$SCRIPT_DIR/forge-env-line.sh" --json --forge "$FORGE" "$DEMO" >"$OUTDIR/env-line.json"
phase gate/environment-line "docs/BENCH.md environment line recorded"

note ""
note "release-gate: environment"
sed 's/^/  /' "$OUTDIR/env-line.txt"
note ""
note "release-gate: PASS - forge $FORGE_VERSION sealed and verified itself as $TAG"
note "release-gate: artifacts in $OUTDIR"
