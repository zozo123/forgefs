# scripts/json-lib.sh
#
# JSON without an interpreter, for the three scripts that ARE the ForgeFS
# self-verification story: release-gate.sh, cli-abi-conformance.sh and
# forge-env-line.sh.
#
# Those scripts used to require `python3` -- for JSON shaping, a byte-fill and
# one SQLite header read, nothing else -- and refused to start without it, with
# exit 2, the code CLI_ABI.md reserves for corruption. A cautious user reaching
# for the gates on a base Debian image got "harness error: python3 is required"
# from the very tools that are supposed to have fewer prerequisites than the
# product they verify (issue #346). Everything the interpreter did is done here
# instead, in the shell and awk the scripts already use.
#
# This file is sourced, never executed. Every function is pure text: none reads
# a repository, none writes anywhere but the caller's own redirections.
#
# Prerequisites, in full: bash, coreutils and awk. Any awk will do; the
# programs below use no GNU extension and are exercised against mawk, which is
# what a Debian base image has.

# Unit separator: the field delimiter these scripts already use for their
# record files, and the one json_top_level emits.
JSON_US=$'\x1f'

# json_string <text>
#
# Print <text> escaped as the BODY of a JSON string -- no surrounding quotes,
# so the caller decides where the quotes go. Newlines become \n and every
# control character is escaped, so the result is always one line and always
# valid between two double quotes.
json_string() {
	printf '%s' "${1-}" | awk '
		BEGIN {
			# \b \f and friends have no shorthand here on purpose: \uXXXX
			# is valid for every one of them, and one rule is easier to
			# trust than eight.
			for (i = 1; i < 32; i++) ctrl[sprintf("%c", i)] = sprintf("\\u%04x", i)
			ctrl[sprintf("%c", 127)] = "\\u007f"
		}
		function esc(s,   i, c, out) {
			out = ""
			for (i = 1; i <= length(s); i++) {
				c = substr(s, i, 1)
				if (c == "\\") out = out "\\\\"
				else if (c == "\"") out = out "\\\""
				else if (c == "\t") out = out "\\t"
				else if (c == "\r") out = out "\\r"
				else if (c in ctrl) out = out ctrl[c]
				else out = out c
			}
			return out
		}
		{
			if (NR > 1) printf "\\n"
			printf "%s", esc($0)
		}
	'
}

# json_field <indent> <key> <text> [,]
#
# One `"key": "escaped text"` member line, with the trailing comma the caller
# asks for. Values are always strings; use json_raw_field for numbers,
# booleans, arrays and objects.
json_field() {
	printf '%*s"%s": "%s"%s\n' "$1" "" "$(json_string "$2")" "$(json_string "$3")" "${4-}"
}

# json_raw_field <indent> <key> <json-text> [,]
#
# One member line whose value is spliced verbatim. The caller owns its
# validity: pass a number, `true`, `false`, `null`, or a balanced array or
# object -- typically one json_top_level handed back.
json_raw_field() {
	printf '%*s"%s": %s%s\n' "$1" "" "$(json_string "$2")" "$3" "${4-}"
}

# json_top_level < FILE
#
# Scan one JSON object and print one line per top-level member:
#
#     <key><US><value as JSON text>
#
# This is a scanner, not a pattern match: it tracks string state and bracket
# depth, so a brace or a colon inside a string value cannot be mistaken for
# structure. Values are emitted verbatim except that literal newlines between
# tokens collapse to spaces -- JSON escapes newlines inside strings, so no
# value's own content is touched, and the result stays one line per member.
#
# Exit 2 if the input is not a JSON object. It deliberately does not validate
# what it skips over: its job is to hand a shell caller the members of a
# document some other program produced, not to be a conformance checker.
json_top_level() {
	awk -v US="$JSON_US" '
		function skipws(i,   c) {
			while (i <= n) {
				c = substr(doc, i, 1)
				if (c == " " || c == "\t" || c == "\n" || c == "\r") i++
				else return i
			}
			return i
		}
		# i indexes the opening quote; returns the index just past the
		# closing one, or 0 if the string never closes.
		function scanstring(i,   c) {
			i++
			while (i <= n) {
				c = substr(doc, i, 1)
				if (c == "\\") { i += 2; continue }
				if (c == "\"") return i + 1
				i++
			}
			return 0
		}
		function scanvalue(i,   c, depth) {
			c = substr(doc, i, 1)
			if (c == "\"") return scanstring(i)
			if (c == "{" || c == "[") {
				depth = 0
				while (i <= n) {
					c = substr(doc, i, 1)
					if (c == "\"") {
						i = scanstring(i)
						if (i == 0) return 0
						continue
					}
					if (c == "{" || c == "[") depth++
					else if (c == "}" || c == "]") {
						depth--
						if (depth == 0) return i + 1
					}
					i++
				}
				return 0
			}
			while (i <= n) {
				c = substr(doc, i, 1)
				if (c == "," || c == "}" || c == "]" ||
				    c == " " || c == "\t" || c == "\n" || c == "\r") return i
				i++
			}
			return i
		}
		{ doc = doc $0 "\n" }
		END {
			n = length(doc)
			i = skipws(1)
			if (substr(doc, i, 1) != "{") exit 2
			i++
			for (;;) {
				i = skipws(i)
				if (i > n) exit 2
				c = substr(doc, i, 1)
				if (c == "}") exit 0
				if (c == ",") { i++; continue }
				if (c != "\"") exit 2
				ks = i
				i = scanstring(i)
				if (i == 0) exit 2
				key = substr(doc, ks + 1, i - ks - 2)
				i = skipws(i)
				if (substr(doc, i, 1) != ":") exit 2
				i = skipws(i + 1)
				vs = i
				i = scanvalue(i)
				if (i == 0) exit 2
				value = substr(doc, vs, i - vs)
				gsub(/\n/, " ", value)
				print key US value
			}
		}
	'
}

# json_member <members> <key>
#
# Print the value of <key> from json_top_level output, or nothing if the
# document had no such member. Exit 1 when the member is absent, so a caller
# can tell "absent" from "present and empty".
json_member() {
	printf '%s\n' "$1" | awk -v US="$JSON_US" -v want="$2" '
		BEGIN { FS = US; found = 0 }
		$1 == want { print substr($0, length($1) + 2); found = 1; exit 0 }
		END { if (!found) exit 1 }
	'
}
