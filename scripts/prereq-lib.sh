# scripts/prereq-lib.sh
#
# The declared prerequisite list of the gate scripts, and the two guards that
# keep the declaration from being aspirational.
#
# scripts/release-gate.sh and scripts/cli-abi-conformance.sh are what a
# cautious user runs BEFORE trusting a release, so their prerequisite list has
# to be shorter than the product they verify -- and, more importantly, true.
# It was neither. `grep` is its own Debian package, not part of coreutils, and
# one `grep -Eq` sat in the middle of the release gate while all three
# declarations said the list was bash, coreutils, sed and awk. On a machine
# without it the gate printed
#
#   scripts/release-gate.sh: line 425: grep: command not found
#   gate: FAIL  gate/conflict-object   Conflict object is missing /^conflict .../
#   EXIT=1
#
# and wrote `"ok": false` into gate-summary.json: a missing TOOL reported as a
# failing PRODUCT (issue #354). #346 had removed exactly that misclassification
# for `python3`; it came back one command over.
#
# Fixing the one `grep` is not the fix. This file makes the class of mistake
# impossible in both directions:
#
#   * require_declared_commands  runs before any assertion and exits 3 naming
#     every declared command this machine does not have, so a DECLARED tool
#     that is absent is a harness error up front instead of a fabricated
#     product failure somewhere in the middle;
#   * command_not_found_handle + prereq_guard  catch the other direction, an
#     UNDECLARED tool that the script reaches for anyway. Bash runs the handler
#     in a subshell when the command sits inside a pipeline or a command
#     substitution, so `exit 3` there would only leave the subshell and the
#     caller would still see a non-zero status and blame the product. The
#     handler therefore also records the name, and prereq_guard -- called from
#     the single function that reports a product failure -- re-reads that
#     record and converts the verdict into exit 3.
#
# Both guards use nothing but bash builtins, so neither can fail because of the
# very thing it is diagnosing.
#
# The declaration is verified rather than asserted: the test
# `crates/forge-cli/tests/gate_scripts_need_no_interpreter.rs` builds a bin
# directory holding symlinks to exactly the commands named below, nothing else,
# and runs both gate scripts with that directory as the whole PATH. An
# undeclared command fails that test at the point of use.

# Every external command the gate scripts may run, in full. Everything here is
# part of bash, coreutils, sed or awk -- that is the rule this list encodes,
# and adding anything else to it is the decision to be argued, not the `grep`
# that quietly appears in a diff.
GATE_REQUIRED_COMMANDS="awk basename bash cat chmod date dirname env head mkdir mktemp od rm sed seq sort tr uname wc"

# Diagnostic prefix and the file a not-found command is recorded in. Each
# script sets these; the marker stays empty until the script has a workdir.
PREREQ_SCRIPT="${PREREQ_SCRIPT:-gate}"
PREREQ_MARKER="${PREREQ_MARKER:-}"

# require_declared_commands - exit 3 naming every declared command that is not
# on PATH. `command -v` is a bash builtin, so this works on the empty PATH it
# exists to diagnose.
require_declared_commands() {
	local cmd missing=""
	for cmd in $GATE_REQUIRED_COMMANDS; do
		command -v "$cmd" >/dev/null 2>&1 || missing="$missing $cmd"
	done
	if [ -n "$missing" ]; then
		printf "%s: harness error: missing prerequisite command(s):%s\n" \
			"$PREREQ_SCRIPT" "$missing" >&2
		printf "%s: prerequisites, in full: %s\n" \
			"$PREREQ_SCRIPT" "$GATE_REQUIRED_COMMANDS" >&2
		exit 3
	fi
}

# Bash calls this for any command it cannot find. Record it and refuse: a tool
# this harness needs and does not declare is a harness bug, and it must never
# be able to disprove an assertion about forge.
command_not_found_handle() {
	if [ -n "$PREREQ_MARKER" ]; then
		printf "%s\n" "$1" >>"$PREREQ_MARKER"
	fi
	printf "%s: harness error: command not found: %s\n" "$PREREQ_SCRIPT" "$1" >&2
	printf "%s: prerequisites, in full: %s\n" \
		"$PREREQ_SCRIPT" "$GATE_REQUIRED_COMMANDS" >&2
	exit 3
}

# True when a command went missing during this run.
prereq_missing() {
	[ -n "$PREREQ_MARKER" ] && [ -s "$PREREQ_MARKER" ]
}

# Called from the one function that reports a PRODUCT failure. If a command was
# not found, the failure that is about to be attributed to forge is really this
# harness missing a tool, so say that and exit 3 instead.
prereq_guard() {
	prereq_missing || return 0
	local line list=""
	while IFS= read -r line; do
		[ -n "$line" ] || continue
		list="$list $line"
	done <"$PREREQ_MARKER"
	printf "%s: harness error: required command(s) not found:%s\n" \
		"$PREREQ_SCRIPT" "$list" >&2
	printf "%s: no assertion about forge was disproved; this is a harness failure\n" \
		"$PREREQ_SCRIPT" >&2
	exit 3
}
