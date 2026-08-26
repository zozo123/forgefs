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
# Fixing the one `grep` is not the fix. This file addresses the class, with two
# SEPARATE mechanisms that must not be confused for one another -- they cover
# different failures, and only one of them is portable.
#
#   1. THE UP-FRONT CHECK over the declared list -- `require_declared_commands`.
#      Before any assertion runs, every command in GATE_REQUIRED_COMMANDS is
#      looked up and the script exits 3 naming whatever is absent. This is what
#      actually fixes #354: `grep` is gone from the gate entirely (the match is
#      done in awk by `ere_match`), and every tool that IS still used is named
#      in the list, so its absence is caught before a gate row can misreport
#      it. It is built from `command -v`, `for` and `[` -- POSIX shell, all
#      builtins -- so it behaves identically on bash 3.2 and bash 5.x, and it
#      cannot fail because of the very thing it is diagnosing: it still exits 3
#      naming `awk` when `awk` itself is what is missing. THIS IS THE PRIMARY
#      MECHANISM.
#
#   2. THE CATCH-ALL BACKSTOP for an UNDECLARED command -- the pairing of
#      `command_not_found_handle` with `prereq_guard`. It covers the case the
#      list cannot: a future tool that some diff quietly starts using and
#      forgets to declare. Bash runs the handler in a subshell when the command
#      sits inside a pipeline or a command substitution, so `exit 3` there only
#      leaves the subshell; the handler therefore also records the name, and
#      `prereq_guard` -- called from the single function that reports a product
#      failure -- re-reads that record and converts the verdict into exit 3.
#
# Mechanism 2 IS NOT PORTABLE, and that is a property of the shell, not a
# shortcut taken here. `command_not_found_handle` arrived in bash 4.0 (2009);
# macOS ships bash 3.2.57, the last GPLv2 release, and on it the hook is never
# called. There is no POSIX substitute for it:
#
#   * an ERR trap with `set -E` does not fire in a condition context, and
#     `if ! ere_match ...` -- the exact shape of the #354 defect -- is a
#     condition context. Measured on bash 3.2.57: no ERR trap for `if ! cmd`
#     and none for `cmd || ...`; only a command substitution fires one.
#   * `$?` cannot stand in either: inside the `then` branch of `if ! cmd` the
#     status has already been inverted to 0, so the 127 is not observable.
#
# So on bash 3.2 an undeclared command degrades to what bash gives for free:
# the tool is still NAMED on stderr by the shell itself ("command not found"),
# but the exit code stays 1 and a `gate: FAIL` row is still printed. That is
# strictly the pre-existing behaviour for a case mechanism 1 does not claim to
# cover -- it is not a regression of #354, which is about a DECLARED-list tool
# and is fixed on every shell. `prereq_backstop_available` reports which of the
# two worlds a given run is in, and the tests assert each mechanism only where
# it exists rather than pretending to parity.
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

# --- Mechanism 1: the up-front check (every shell) ---------------------------
#
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

# --- Mechanism 2: the catch-all backstop (bash >= 4 only) --------------------
#
# True when this shell can trap a command it cannot find, i.e. when the
# undeclared-command backstop below is in force. `command_not_found_handle` is
# a bash 4.0 feature; on bash 3.2 (macOS) it is never called, so installing it
# there would be dead code that reads like protection. It is defined only where
# it works, and this predicate is the single place that says where that is.
prereq_backstop_available() {
	[ "${BASH_VERSINFO[0]:-0}" -ge 4 ]
}

if prereq_backstop_available; then
	# Bash calls this for any command it cannot find. Record it and refuse: a
	# tool this harness needs and does not declare is a harness bug, and it
	# must never be able to disprove an assertion about forge.
	command_not_found_handle() {
		if [ -n "$PREREQ_MARKER" ]; then
			printf "%s\n" "$1" >>"$PREREQ_MARKER"
		fi
		printf "%s: harness error: command not found: %s\n" \
			"$PREREQ_SCRIPT" "$1" >&2
		printf "%s: prerequisites, in full: %s\n" \
			"$PREREQ_SCRIPT" "$GATE_REQUIRED_COMMANDS" >&2
		exit 3
	}
fi

# True when a command went missing during this run. Always false on a shell
# without the backstop, because nothing can record into the marker there.
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
