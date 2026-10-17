#!/usr/bin/env bash
# The fixtures are crate prose, whose backticks are Markdown code spans, not command substitution.
# shellcheck disable=SC2016
#
# The regression battery for `run.sh`, the § citation guard beside this file.
#
# Each fixture is a few lines of crate prose written to a scratch file and checked with
# `run.sh SPEC FILE` against the committed clause indexes. It asserts the exit code the guard must
# return and, where the reason matters, a fragment it must print -- so a fixture that starts
# failing for a different reason is caught rather than counted as a pass. Legal citations are
# fixtures too, with expected code 0: a guard that rejects correct prose is a guard someone turns
# off. Not part of `check-citations`, and not in CI: it is for whoever changes the guard.
#
# Usage:
#     tooling/check-citations/fixtures.sh
set -uo pipefail

root="$(git rev-parse --show-toplevel)" || {
    echo "fixtures: not inside a git repository"
    exit 1
}
cd "$root" || exit 1

guard="./tooling/check-citations/run.sh"
work="$(mktemp -d)" || exit 1
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# check <name> <spec> <expected exit> <expected output fragment, or ""> <file text>
check() {
    name="$1"
    spec="$2"
    want="$3"
    fragment="$4"
    printf '%s\n' "$5" >"$work/fixture.rs"
    out="$("$guard" "$spec" "$work/fixture.rs" 2>&1)"
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

ICC=icc.1-2022-05
PNG=png-3

# The repository itself passes (KNOWN in force).
if out="$("$guard" 2>&1)"; then passed=$((passed + 1)); else
    echo "FAIL  the repository passes"
    printf '%s\n' "$out" | sed 's/^/        /'
    failed=$((failed + 1))
fi

# Existence.
check "an existing clause passes" $ICC 0 "1 checked" '/// The `cicpType` (§10.3) payload.'
check "a missing clause is reported" $ICC 1 "no clause heading numbered 10.99 in $ICC" \
    '/// See §10.99.'
check "an annex letter is a clause" $ICC 0 "" '/// PCS encodings (§A.3, §D).'

# Attached names (the PR #542 and #604 defects).
check "a name heading another clause is reported" $ICC 1 \
    '`cicpType` heads 10.3' '/// The `cicpType` (§10.7) payload.'
check "the heading numbering wins over the cross-reference numbering" $ICC 1 \
    '`redMatrixColumnTag` heads 9.2.46' '/// redMatrixColumnTag (§9.2.44)'
check "tRNS cited as cHRM's clause is reported" $PNG 1 '`tRNS` heads 11.3.1.1' \
    '/// tRNS (§11.3.2.1) carries the colour key.'
check "a qualified attached name is checked" $ICC 1 '`iCCP` heads 11.3.2.3' \
    '/// iCCP (PNG §11.3.2.4)'
check "names paired with a citation list match any clause in it" $ICC 0 "2 checked" \
    '//! `curveType` and `parametricCurveType` (ICC.1:2022 §10.6, §10.18).'
check "a name heading no clause of its list is reported" $ICC 1 \
    '`parametricCurveType` heads 10.18' '//! `parametricCurveType` (§10.6, §10.7).'
check "a name no heading carries is not checked" $ICC 0 "" '/// `FooBar` (§10.7)'
check "a name beside a citation is not attached" $PNG 0 "" \
    '/// the `iTXt` §11.3.3.1 Table 21 reserves for it'

# Which specification.
check "an ICC.1:2001-04 designator selects that edition" $ICC 0 "" \
    '/// textDescriptionType (ICC.1:2001-04 §6.5.17)'
check "an unqualified v2 clause means the crate's edition" $ICC 1 \
    "no clause heading numbered 6.5.17 in $ICC" '/// textDescriptionType (§6.5.17)'
check "a PNG designator in an ICC crate selects PNG" $ICC 0 "" '/// iCCP (PNG §11.3.2.3)'
check "an ICC.1:2022 designator in a PNG crate selects ICC" $PNG 0 "" \
    '/// the data colour space (ICC.1:2022 §7.2.6)'
check "a digit-bearing designator is foreign" $ICC 0 "1 into a specification with none here" \
    '/// ITU-T H.273 §8.3 code points'
check "a closing-parenthesis designator is foreign" $ICC 0 "1 into a specification" \
    '/// ITU-T H.273 (07/2024) §8.99'
check "a section-cited standard earlier on the line is foreign" $ICC 0 "1 into a specification" \
    '/// Adobe RGB (1998) Color Image Encoding §4.3.1.2: the inverse'
check "a format named in prose does not switch the check off" $PNG 1 \
    "no clause heading numbered 11.9" '/// An XMP packet; §11.9 gives it'
check "a list inherits its first designator" $PNG 0 "2 into a specification" \
    '//! (C2PA 2.4 §A.3.2, §18.5.4)'
check "a dash range inherits too" $PNG 0 "2 checked" '/// PNG §9.2–§9.4'
check "a wrapped list continues onto the next line" $PNG 0 "2 into a specification" \
    "$(printf '%s\n%s' '//! carriage (C2PA 2.4 §A.3.2,' '//! §18.5.4). Exact-byte')"
check "a wrapped designator qualifies the next line" $PNG 0 "1 checked" \
    "$(printf '%s\n%s' '/// reads the profile (ICC.1:2022' '/// §7.2.6, bytes 16-19)')"
check "an unqualified citation after prose means the crate spec" $PNG 1 \
    "no clause heading numbered A.3.2 in $PNG" '/// a store precedes IDAT (§A.3.2)'

echo "fixtures: $passed passed, $failed failed"
[ "$failed" = 0 ]
