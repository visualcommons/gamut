#!/usr/bin/env bash
# Drift guard for the fuzz tier's three hand-maintained lists (issues #264, #311).
#
# A target exists in three places that nothing reconciles:
#
#   1. tooling/gamut-fuzz/fuzz_targets/<name>.rs   -- the code
#   2. tooling/gamut-fuzz/Cargo.toml [[bin]]       -- what cargo-fuzz can build and list
#   3. .github/workflows/extended.yml fuzz matrix  -- what CI actually runs
#
# Miss (2) and `mise run fuzz <name>` says the target does not exist. Miss (3) and the target is
# written, reviewed, committed -- and never run, silently, because nothing anywhere fails. This
# script is the thing that fails. It is pure text: no cargo, no toolchain, no network, which is
# why CI runs it in the cheap `Format & Metadata` job rather than in lint.
set -euo pipefail

cd "$(dirname "$0")/../.."

MANIFEST="tooling/gamut-fuzz/Cargo.toml"
WORKFLOW=".github/workflows/extended.yml"
TARGET_DIR="tooling/gamut-fuzz/fuzz_targets"

# (1) The files on disk.
files="$(find "$TARGET_DIR" -maxdepth 1 -name '*.rs' -printf '%f\n' | sed 's/\.rs$//' | sort)"

# (2) The `[[bin]]` entries. Read the name/path pair per section so a `name` that disagrees with
# its own `path` -- a copy-paste that builds the wrong file under the right label -- is caught
# too, not just a missing section.
bins="$(
	awk '
		/^\[\[bin\]\]/            { name = ""; path = ""; next }
		/^\[/                     { name = ""; path = ""; next }
		/^name = "/               { name = $3; gsub(/"/, "", name) }
		/^path = "fuzz_targets\// { path = $3; gsub(/"|fuzz_targets\/|\.rs/, "", path) }
		name != "" && path != ""  { if (name != path) { print "[[bin]] name \"" name "\" builds fuzz_targets/" path ".rs" > "/dev/stderr"; exit 1 }
		                            print name; name = ""; path = "" }
	' "$MANIFEST" | sort
)"

# (3) The workflow matrix. The list is a plain YAML sequence under the fuzz job's `target:` key;
# comment lines inside it are skipped, and the first line that is neither ends the list.
matrix="$(
	awk '
		/^  fuzz:$/          { job = 1 }
		job && /^ *target:$/ { list = 1; next }
		list {
			if ($0 ~ /^ *#/)                    { next }
			if ($0 ~ /^ *- [A-Za-z0-9_]+$/)     { print $2; next }
			exit
		}
	' "$WORKFLOW" | sort
)"

status=0

# A hand-maintained list can name the same target twice, and `comm -23` reports the second copy as
# a line present on the left and absent on the right -- i.e. as "a [[bin]] points at a file that
# does not exist" or "CI names a target that cannot be built", neither of which is what happened.
# Duplicates are therefore diagnosed first, by name, and the lists are de-duplicated before the
# set comparisons below so those keep saying what they mean. (`find` cannot produce a duplicate
# filename, so (1) needs no such check.)
duplicates() {
	local what="$1" where="$2" list="$3" dupes
	dupes="$(printf '%s\n' "$list" | uniq -d)"
	if [ -n "$dupes" ]; then
		echo "$where names the same fuzz target more than once ($what):" >&2
		printf '  %s\n' $dupes >&2
		status=1
	fi
}

duplicates "cargo would build it twice" "$MANIFEST" "$bins"
duplicates "CI would run it twice" "$WORKFLOW" "$matrix"
bins="$(printf '%s\n' "$bins" | uniq)"
matrix="$(printf '%s\n' "$matrix" | uniq)"

report() {
	local what="$1" left="$2" right="$3" left_list="$4" right_list="$5"
	local missing
	missing="$(comm -23 <(printf '%s\n' "$left_list") <(printf '%s\n' "$right_list"))"
	if [ -n "$missing" ]; then
		echo "fuzz target(s) in $left but not in $right ($what):" >&2
		printf '  %s\n' $missing >&2
		status=1
	fi
}

report "the target would never be built" "$TARGET_DIR" "$MANIFEST" "$files" "$bins"
report "a [[bin]] points at a file that does not exist" "$MANIFEST" "$TARGET_DIR" "$bins" "$files"
report "the target would never run in CI" "$MANIFEST" "$WORKFLOW" "$bins" "$matrix"
report "CI names a target that cannot be built" "$WORKFLOW" "$MANIFEST" "$matrix" "$bins"

if [ "$status" -ne 0 ]; then
	exit 1
fi

echo "fuzz targets in step: $(printf '%s\n' "$files" | wc -l) in $TARGET_DIR, $MANIFEST and $WORKFLOW"
