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
#   2  harness failure: bad usage, missing binary, missing python3
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
command -v python3 >/dev/null 2>&1 || harness_die "python3 is required"
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
: >"$PHASES"
: >"$FAILURES"
US=$'\x1f'
START_EPOCH="$(date +%s)"

# Capability tokens ARE authority (I13/I14) and must never reach a log line or
# a CI artifact, not even a throwaway fixture's token.
redact() {
	sed -e 's/fmac1_[0-9a-fA-F]*/fmac1_<redacted-capability-token>/g'
}

note() { printf '%s\n' "$*"; }

phase() {
	printf '%s%s%s\n' "$1" "$US" "$(printf '%s' "$2" | redact | tr '\n' ' ')" >>"$PHASES"
	printf 'gate: ok    %-38s %s\n' "$1" "$(printf '%s' "$2" | redact | tr '\n' ' ')"
}

fail() {
	local id="$1" detail="$2"
	printf '%s%s%s\n' "$id" "$US" "$(printf '%s' "$detail" | redact | tr '\n' ' ')" >>"$FAILURES"
	printf 'gate: FAIL  %-38s %s\n' "$id" "$(printf '%s' "$detail" | redact | tr '\n' ' ')" >&2
}

# Always emit the summary, including on failure, so the artifact is diagnosable.
finish() {
	local status=$?
	local failed
	failed="$(wc -l <"$FAILURES" | tr -d ' ')"
	GATE_PHASES="$PHASES" \
		GATE_FAILURES="$FAILURES" \
		GATE_OUT="$OUTDIR/gate-summary.json" \
		GATE_FORGE="$FORGE" \
		GATE_FORGE_VERSION="$FORGE_VERSION" \
		GATE_TAG="$TAG" \
		GATE_STATUS="$status" \
		GATE_FAILED_COUNT="$failed" \
		GATE_STARTED="$START_EPOCH" \
		GATE_OUTDIR="$OUTDIR" \
		python3 - <<'PY' || true
import json
import os
import time

sep = "\x1f"


def load(path):
    rows = []
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                line = line.rstrip("\n")
                if not line:
                    continue
                name, _, detail = line.partition(sep)
                rows.append({"id": name, "detail": detail})
    except FileNotFoundError:
        pass
    return rows


outdir = os.environ["GATE_OUTDIR"]


def artifact(name):
    path = os.path.join(outdir, name)
    return {"file": name, "present": os.path.exists(path)}


def read_json(name):
    try:
        with open(os.path.join(outdir, name), "r", encoding="utf-8") as handle:
            return json.load(handle)
    except Exception:
        return None


fsck = read_json("fsck-full.json")
abi = read_json("abi-conformance.json")
env = read_json("env-line.json")
failed = int(os.environ["GATE_FAILED_COUNT"])
status = int(os.environ["GATE_STATUS"])

summary = {
    "schema": "forgefs.release-gate/1",
    "ok": failed == 0 and status == 0,
    "script_exit_status": status,
    "forge_binary": os.environ["GATE_FORGE"],
    "forge_version": os.environ["GATE_FORGE_VERSION"],
    "seal_tag": os.environ["GATE_TAG"],
    "started_unix": int(os.environ["GATE_STARTED"]),
    "duration_seconds": int(time.time()) - int(os.environ["GATE_STARTED"]),
    "phases_passed": load(os.environ["GATE_PHASES"]),
    "failures": load(os.environ["GATE_FAILURES"]),
    "fsck": (
        None
        if fsck is None
        else {
            "ok": fsck.get("ok"),
            "full": fsck.get("full"),
            "checked_refs": fsck.get("checked_refs"),
            "checked_objects": fsck.get("checked_objects"),
            "checked_namespaces": fsck.get("checked_namespaces"),
            "findings": fsck.get("findings"),
        }
    ),
    "cli_abi": (
        None
        if abi is None
        else {
            "ok": abi.get("ok"),
            "rows_total": abi.get("rows_total"),
            "rows_blocking": abi.get("rows_blocking"),
            "rows_known_failing": abi.get("rows_known_failing"),
            "rows_unexercised": abi.get("rows_unexercised"),
            "blocking_failures": abi.get("blocking_failures"),
            "known_failing": [
                {
                    "id": r["id"],
                    "contract_exit": r["contract_exit"],
                    "observed_exit": r["observed_exit"],
                    "note": r["note"],
                }
                for r in abi.get("rows", [])
                if r.get("class") == "known_failing"
            ],
        }
    ),
    "environment": env,
    "artifacts": [
        artifact(n)
        for n in (
            "fsck-full.json",
            "seal-attestation.txt",
            "abi-conformance.json",
            "env-line.txt",
            "env-line.json",
            "conflict-object.txt",
        )
    ],
}
with open(os.environ["GATE_OUT"], "w", encoding="utf-8") as handle:
    json.dump(summary, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
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
for want in "^conflict $CONFLICT_OID" "^ours [0-9a-f]{64}$" "^theirs [0-9a-f]{64}$" "^path /?overlap\.txt "; do
	if ! printf '%s\n' "$CONFLICT_SHOW" | grep -Eq "$want"; then
		fail gate/conflict-object "Conflict object is missing /$want/: $CONFLICT_SHOW"
		exit 1
	fi
done
CONFLICT_OURS="$(printf '%s\n' "$CONFLICT_SHOW" | awk '/^ours /{print $2; exit}')"
CONFLICT_THEIRS="$(printf '%s\n' "$CONFLICT_SHOW" | awk '/^theirs /{print $2; exit}')"
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
# Phase 7 - fsck --full --json, parsed as JSON (never grepped as prose)
# ---------------------------------------------------------------------------
must gate/fsck --dir "$DEMO" --cap "$ROOT" fsck --full --json
printf '%s\n' "$LAST_OUT" >"$OUTDIR/fsck-full.json"
FSCK_SUMMARY="$(FSCK_PATH="$OUTDIR/fsck-full.json" python3 - <<'PY'
import json
import os
import sys

with open(os.environ["FSCK_PATH"], "r", encoding="utf-8") as handle:
    report = json.load(handle)

problems = []
if report.get("ok") is not True:
    problems.append("ok is %r, expected True" % (report.get("ok"),))
if report.get("full") is not True:
    problems.append("full is %r, expected True" % (report.get("full"),))
findings = report.get("findings")
if findings:
    problems.append("findings is non-empty: %s" % json.dumps(findings))
for key in ("checked_refs", "checked_objects", "checked_namespaces"):
    value = report.get(key)
    if not isinstance(value, int) or value <= 0:
        problems.append("%s is %r, expected a positive integer" % (key, value))

if problems:
    sys.stderr.write("; ".join(problems) + "\n")
    sys.exit(1)

print(
    "refs=%d objects=%d namespaces=%d findings=0"
    % (
        report["checked_refs"],
        report["checked_objects"],
        report["checked_namespaces"],
    )
)
PY
)" || {
	fail gate/fsck "fsck --full --json report is not clean (see fsck-full.json)"
	exit 1
}
phase gate/fsck "fsck --full --json ok: $FSCK_SUMMARY"

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
