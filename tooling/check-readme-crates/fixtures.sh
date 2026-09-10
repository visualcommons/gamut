#!/usr/bin/env bash
# The regression battery for `run.sh`, the README crates-table guard beside this file.
#
# The guard grew five checks over four rounds of review, and each round proved itself with a
# battery of single-edit fixtures that lived only in the pull request description. A claim in a
# description is not a regression test: nobody can re-run it, and the next round starts from an
# assertion rather than from an execution. This file is that battery, committed.
#
# Each fixture is ONE edit to the repository README, applied to a copy. The fixture asserts the
# exit code the guard must return for it and, where the reason matters, a fragment the guard must
# print -- so a fixture that starts failing for a different reason is caught rather than counted
# as a pass. A fixture is named for the single property it holds the guard to.
#
# The three groups are the guard's three jobs: that the table lists the crates (membership and row
# shape), that it renders as one table (structure), and that the claims its prose makes agree with
# `cargo metadata` (the claim forms). Legal renderings are fixtures too, with expected code 0: a
# guard that rejects legal prose is a guard someone turns off, and both over-rejections this
# battery pins were found by a reviewer after they shipped.
#
# Usage:
#     tooling/check-readme-crates/fixtures.sh            # system awk
#     CHECK_README_AWK='gawk --posix' tooling/check-readme-crates/fixtures.sh
#
# With no argument the fixtures are cut from the repository's own README, so they follow it as it
# changes; pass a path to cut them from another file instead.
set -uo pipefail

root="$(git rev-parse --show-toplevel)" || {
    echo "fixtures: not inside a git repository"
    exit 1
}
cd "$root" || exit 1

guard="./tooling/check-readme-crates/run.sh"
base="${1:-README.md}"

test -f "$base" || {
    echo "fixtures: no such file: $base"
    exit 1
}

# The guard must agree with the tree before any fixture means anything: every fixture is a single
# edit AWAY from this file, so a base that already fails would make every expectation unreadable.
work="$(mktemp -d)" || exit 1
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# check <name> <expected exit> <expected output fragment, or ""> <transform...>
# The transform reads the base README on stdin and writes the fixture on stdout.
check() {
    name="$1"
    want="$2"
    fragment="$3"
    shift 3

    "$@" <"$base" >"$work/README.md" || {
        echo "FAIL  $name -- the fixture transform itself failed"
        failed=$((failed + 1))
        return
    }

    out="$("$guard" "$work/README.md" 2>&1)"
    got=$?

    if [ "$got" != "$want" ]; then
        echo "FAIL  $name -- expected exit $want, got $got"
        printf '%s\n' "$out" | sed 's/^/        /'
        failed=$((failed + 1))
        return
    fi
    if [ -n "$fragment" ] && ! printf '%s\n' "$out" | grep -qF -- "$fragment"; then
        echo "FAIL  $name -- exit $got as expected, but the output does not say '$fragment'"
        printf '%s\n' "$out" | sed 's/^/        /'
        failed=$((failed + 1))
        return
    fi
    passed=$((passed + 1))
}

# Transforms. Each reads stdin and writes stdout; `sedx`/`awkx` keep the fixture table to one
# readable line apiece.
unchanged() { cat; }
sedx() { sed "$1"; }
awkx() { awk "$1"; }

# ---------------------------------------------------------------------------------------------
# Membership and row shape: the table lists every crate, once, in a row a reader can read.
# ---------------------------------------------------------------------------------------------
check "baseline: the repository README passes"            0 "lists every workspace crate" unchanged
check "a crate row deleted"                               1 "no row in"                   sedx '/^| `gamut-png` /d'
check "a crate row misspelled into a phantom"             1 "not workspace members"       sedx 's/^| `gamut-png` /| `gamut-pngg` /'
check "a crate listed twice"                              1 "listed more than once"       sedx '/^| `gamut-png` /p'
check "a crate row hidden in an HTML comment"             1 "no row in"                   awkx '/^\| `gamut-png` /{print "<!--"; print; print "-->"; next} {print}'
check "a crate row hidden in a fenced code block"         1 "no row in"                   awkx '/^\| `gamut-png` /{print "```"; print; print "```"; next} {print}'
check "a phantom row hiding behind a capital"             1 "not workspace members"       sedx 's/^| `gamut-png` /| `Gamut-png` /'
check "a phantom row hiding behind an underscore"         1 "not workspace members"       sedx 's/^| `gamut-png` /| `gamut_png` /'
check "a two-cell crate row"                              1 "malformed crate rows"        sedx 's/^| `gamut-png` .*/| `gamut-png` | only one cell |/'
check "a four-cell crate row"                             1 "malformed crate rows"        sedx '/^| `gamut-png` /s/$/ extra |/'
check "an empty Status cell"                              1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | |/'
check "a Status cell of one &nbsp;"                       1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&nbsp; |/'
check "a Status cell of one &lrm;"                        1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&lrm; |/'
check "a Status cell of one &#8206;"                      1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&#8206; |/'
check "a Status cell of one &#08206; (leading zero)"      1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&#08206; |/'
check "a Status cell of one &#x200E;"                     1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&#x200E; |/'
check "a Status cell of one &#5760;"                      1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&#5760; |/'
check "a Status cell of one &#x1680;"                     1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | \&#x1680; |/'
check "a Status cell of one non-breaking space"           1 "empty Status cell"           awkx '{ if ($0 ~ /^\| `gamut-png` /) { print "| `gamut-png` | PNG codec | \302\240 |"; next } print }'
check "a Status cell of one tab"                          1 "empty Status cell"           awkx '{ if ($0 ~ /^\| `gamut-png` /) { print "| `gamut-png` | PNG codec |\t|"; next } print }'
check "a Status cell of one empty HTML element"           1 "empty Status cell"           sedx 's/^| `gamut-png` .*/| `gamut-png` | PNG codec | <span><\/span> |/'
check "a crate row with no trailing pipe"                 1 "no row in"                   sedx '/^| `gamut-png` /s/|$//'
check "a crate row indented four spaces (a code block)"   1 "no row in"                   sedx '/^| `gamut-png` /s/^/    /'
check "a crate row indented two spaces (legal)"           0 "lists every workspace crate" sedx '/^| `gamut-png` /s/^/  /'

# ---------------------------------------------------------------------------------------------
# Structure: every crate row renders inside ONE table, whatever the bytes of the row look like.
# ---------------------------------------------------------------------------------------------
check "the delimiter row deleted"                         1 "do not render inside a table" sedx '/^| -----/d'
check "a blank line directly after the delimiter"         1 "do not render inside a table" sedx '/^| -----/G'
check "a blank line between two crate rows"               1 "do not render inside a table" sedx '/^| `gamut-png` /G'
check "the header row blanked"                            1 "do not render inside a table" sedx 's/^| Crate .*/|  |  |  |/'
check "a two-column header over a three-column delimiter" 1 "do not render inside a table" sedx 's/^| Crate .*/| Crate | Purpose |/'
check "the delimiter row indented four spaces"            1 "do not render inside a table" sedx '/^| -----/s/^/    /'
check "an earlier table donates its delimiter"            1 "do not render inside a table" awkx '/^\| Crate /{print "| Decoy | Decoy | Decoy |"; print "| --- | --- | --- |"; print ""} /^\| -----/{next} {print}'
check "a decoy table whose own first row is a crate row"  1 "do not render inside a table" awkx '/^\| Crate /{print "| `gamut-png` | PNG codec | shipped |"; print "| --- | --- | --- |"; print ""} /^\| -----/{next} /^\| `gamut-png` /{next} {print}'
check "the crate rows split over two valid tables"        1 "spread over"                  awkx '/^\| `gamut-png` /{print ""; print "| Crate | Purpose | Status |"; print "| --- | --- | --- |"} {print}'
check "alignment colons in the delimiter (legal)"         0 "lists every workspace crate"  awkx '/^\| -----/{print "|:---|:---:|---:|"; next} {print}'
check "an escaped pipe inside a cell (legal)"             0 "lists every workspace crate"  awkx '{ if ($0 ~ /^\| `gamut-riff` /) sub(/RIFF container utilities \(WebP\)/, "RIFF a \\| b"); print }'
check "a non-crate separator row inside the table (legal)" 0 "lists every workspace crate" awkx '/^\| `gamut-png` /{print "| **Codecs** |  |  |"} {print}'
check "an unrelated table earlier in the section (legal)" 0 "lists every workspace crate"  awkx '/^## Crates$/{print; print ""; print "| A | B |"; print "| --- | --- |"; print "| 1 | 2 |"; print ""; next} {print}'

# ---------------------------------------------------------------------------------------------
# The section heading, in every form CommonMark gives it.
# ---------------------------------------------------------------------------------------------
check "the heading with a closing sequence (legal)"       0 "lists every workspace crate" sedx 's/^## Crates$/## Crates ##/'
check "the heading indented three spaces (legal)"         0 "lists every workspace crate" sedx 's/^## Crates$/   ## Crates/'
check "the heading in setext form (legal)"                0 "lists every workspace crate" awkx '{ if ($0 == "## Crates") { print "Crates"; print "------"; next } print }'
check "a trailing space on the heading (legal)"           0 "lists every workspace crate" sedx 's/^## Crates$/## Crates /'
check "a CRLF file (legal)"                               0 "lists every workspace crate" sedx 's/$/\r/'
check "a heading whose text is Crates#, not Crates"       1 "found no crate rows"         sedx 's/^## Crates$/## Crates#/'

# ---------------------------------------------------------------------------------------------
# The claim forms. Each is opt-in and marker-driven; each fixture writes one claim cargo refutes,
# and each is paired with the legal claim it must not reject.
# ---------------------------------------------------------------------------------------------
check "a version token contradicting the manifest"        1 "version claims"   sedx '/^| `gamut-core` /s/stable (v2;/stable (v3;/'
check "a minor version contradicting the manifest"        1 "version claims"   sedx '/^| `gamut-bitstream` /s/v0.2,/v0.3,/'
check "a consumed-by list one crate short"                1 "consumed by"      sedx 's/`gamut-png` and `gamut-webp`;/`gamut-png`;/'
check "a consumed-by list naming a non-consumer"          1 "consumed by"      sedx 's/`gamut-png` and `gamut-webp`;/`gamut-png`, `gamut-riff` and `gamut-webp`;/'
check "an always-on list naming an optional edge"         1 "always-on"        sedx 's/always-on dependency: `gamut-core`/always-on dependency: `gamut-avif`/'
check "an always-on list naming an edge that is absent"   1 "always-on"        sedx 's/always-on dependency: `gamut-core`/always-on dependency: `gamut-jxl-sys`/'
check "a feature no crate the row names declares"         1 "feature claims"   sedx 's/behind Cargo feature `codec-abi`/behind Cargo feature `codec-abii`/'
check "an off-by-default feature called a default one"    1 "feature claims"   sedx 's/BigTIFF behind Cargo feature `bigtiff`/BigTIFF behind default Cargo feature `bigtiff`/'
check "a default feature owned by a crate not named here" 1 "feature claims"   sedx '/^| `gamut-riff` /s/stable (v1, #186)/stable (v1, #186); default Cargo feature `decode`/'
check "another real feature of the row crate (legal)"     0 "lists every"      sedx 's/BigTIFF behind Cargo feature `bigtiff`/BigTIFF behind Cargo feature `bigtiff`, with Cargo feature `test-support` for fixtures/'
check "a feature of another crate the row names (legal)"  0 "lists every"      sedx 's/behind Cargo feature `codec-abi`/behind Cargo feature `codec-abi` and Cargo feature `avif`/'
check "a cell naming a crate that does not exist"         1 "do not exist"     sedx 's/built on `gamut-ifd`/built on `gamut-ifdd`/'
check "a cell naming a real crate (legal)"                0 "lists every"      sedx '/^| `gamut-riff` /s/(WebP)/(WebP), read by `gamut-webp`/'
check "an external dependency cargo does not record"      1 "external-dependency" sedx 's/external dependency `miniz_oxide`/external dependency `miniz_oxidee`/'
check "an external dependency no crate in the row has"    1 "external-dependency" sedx 's/external dependency `miniz_oxide`/external dependency `serde`/'

# The two over-rejections a reviewer found after they shipped. Both are legal prose and both must
# stay legal: bare `features` is an English verb, which is why the marker carries `Cargo`.
check "the word features as an English verb (legal)"      0 "lists every"      sedx '/^| `gamut-riff` /s/(WebP)/(WebP); the crate features `chunk` walking/'
check "the underscore spelling of a real crate"           1 "do not exist"     sedx 's/built on `gamut-ifd`/built on `gamut_ifd`/'

# ---------------------------------------------------------------------------------------------
# The precondition the name checks rest on, and the section prose they now reach.
# ---------------------------------------------------------------------------------------------
check "a crate name in a cell without a code span"        1 "without a code span" sedx 's/built on `gamut-ifd`/built on gamut-ifd/'
check "a crate name in section prose without a code span" 1 "without a code span" sedx 's/^Each crate manifest sets/Each gamut-png crate manifest sets/'
check "the bare word gamut in prose (legal)"              0 "lists every"         sedx 's/^Each crate manifest sets/Each gamut crate manifest sets/'
check "a crate name inside a fence in the section (legal)" 0 "lists every"        awkx '/^## Prerequisites/{print "```rust"; print "use gamut_png::Encoder;"; print "```"; print ""} {print}'
check "a phantom crate named in section prose"            1 "do not exist"        sedx 's/^Each crate manifest sets/Each `gamut-ifdd` crate manifest sets/'
check "a feature claim in section prose cargo refutes"    1 "feature claims"      sedx 's/^Each crate manifest sets/The `gamut-png` Cargo feature `nope` is gone; each crate manifest sets/'
check "a consumed-by list outside a crate row"            1 "no crate to be about" sedx 's/^Each crate manifest sets/Each is consumed by `gamut-png`; each crate manifest sets/'
check "an always-on list outside a crate row"             1 "no crate to be about" sedx 's/^Each crate manifest sets/Each has an always-on dependency: `gamut-core`; each crate manifest sets/'
check "a version token outside a crate row"               1 "no crate to be about" sedx 's/^Each crate manifest sets/Each v9 crate manifest sets/'

echo
echo "check-readme-crates fixtures: $passed passed, $failed failed (awk: ${CHECK_README_AWK:-awk})"
test "$failed" -eq 0
