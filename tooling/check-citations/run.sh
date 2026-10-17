#!/usr/bin/env bash
# A `§X.Y` citation names a clause of a vendored specification, and nothing compared the two but a
# human reading a diff: `gamut-png` cited `tRNS` as §11.3.2.1, which PNG gives to `cHRM` (#604),
# and `gamut-icc` cited §10.7 for `cicpType` and §9.2.10/§9.2.35 for tags those clauses do not
# define (PR #542) -- the last beside a hand audit's claim that every other citation checked out.
# This guard is the check that claim needed (issue #606).
#
# WHAT IT READS: only the committed clause indexes, `references/<spec>/clauses.tsv` (columns
# `spec`, `clause`, `title`), and the tracked `.rs`/`.md`/`.toml` files of the crates in CRATES
# below. It builds nothing and runs no PDF tool: `tooling/spec-clauses` generates the TSVs and
# `mise run check-spec-clauses` holds them to the specifications. The numbering checked against is
# the specification's *headings*; where its own cross-references number a clause differently
# (ICC.1:2022 §8.4.3 calls `redMatrixColumnTag` 9.2.44, its heading 9.2.46) a citation copying the
# cross-reference is reported, because the heading is the only numbering a reader can look up.
#
# WHICH SPECIFICATION a citation means is read from the citing line's own prose, per citation, in
# this order (the first rule that applies decides):
#
#   1. a citation that follows another one on the same line, separated only by commas, slashes,
#      dashes, semicolons, spaces and `and`/`or`/`to`/`through`, means the same one
#      (`PNG §9.2–§9.4`, `(ICC.1:2022 §9.2.31, §9.2.46)`, `C2PA 2.4 §A.3.2, §18.5.4`);
#   2. the word directly before the `§` (one space between) -- or, for a citation that opens its
#      line after only comment markup, the last word of the line before (a wrapped
#      `(ICC.1:2022` / `§7.2.6`) -- names it when it is a designator: `ICC.1:2022` or `ICC.1` ->
#      ICC.1:2022, any `ICC.1:2001...` -> ICC.1:2001-04, `PNG` -> PNG Third Edition. Any other
#      designator -- a word holding a digit (`H.273`, `RFC 1951`, `C2PA 2.4`), closing a
#      parenthesis (`H.273 (07/2024)`), or a foreign standard's name (`C2PA`, `ISO`, `TIFF`, ...)
#      -- names a specification with no index here: the citation is counted as foreign, unchecked;
#   3. a foreign standard these crates cite by section (`C2PA`, `Adobe RGB`, `H.273`, `RFC`) named
#      anywhere earlier on the line makes it foreign too (`Adobe RGB (1998) Color Image Encoding
#      §4.3.1.2`). Only those four: a format named in prose (`an XMP packet ... §11.3.3.4`) is a
#      subject, not a citation's source, and must not switch a check off;
#   4. otherwise it means the crate's own specification (the CRATES table).
#
# WHAT IS CHECKED, for every citation that resolves to an indexed specification:
#
#   * the clause exists among that specification's headings;
#   * where a name is attached to the citation by a parenthesis -- `NAME (§N` or `NAME (PNG §N` --
#     and NAME (backticks stripped) is identifier-shaped (four or more characters with an inner
#     capital or digit: `tRNS`, `cicpType`, `IHDR`, `s15Fixed16Number`) and appears as a word in
#     *some* heading of that specification, the cited heading is one of them -- for a list of
#     citations in the parenthesis (`curveType` and `parametricCurveType` (ICC.1:2022 §10.6,
#     §10.18)), at least one heading in the list. A name no heading
#     carries says nothing about the clause, so it is not checked. A name merely *beside* a
#     citation (`the `iTXt` §11.3.3.1 Table 21 reserves`) is not attached: the clause often
#     belongs to the words after it.
#
# WHAT IS NOT CHECKED: a name in prose that is not attached as above (`the MatrixColumn tags
# (§9.2.10)` is not identifier-shaped), a citation into a specification with no index here, and
# whether the cited clause actually says what the sentence claims. So a clean run means "every
# citation names a heading that exists, and every attached name heads the clause cited" -- not that
# the citations are apt.
#
# KNOWN lists the findings present when the guard landed, as `path <TAB> clause` pairs, so the guard
# could gate from its first commit; each is a citation to fix, not a ruling that it is right. A
# KNOWN pair that no longer fires is itself reported, so the list can only shrink. The pair is
# coarser than a line on purpose (line numbers drift under every unrelated edit), and the price is
# that while a pair is listed, a *new* bad citation of that clause in that file is masked too.
#
# Usage:
#     tooling/check-citations/run.sh                    # the CRATES below, from the repository root
#     tooling/check-citations/run.sh SPEC FILE...       # FILEs whose bare citations mean SPEC
#                                                       # (a clauses.tsv `spec` id); KNOWN unused
set -euo pipefail

root="$(git rev-parse --show-toplevel)" || {
    echo "check-citations: not inside a git repository"
    exit 1
}

# crate directory -> the spec id (a clauses.tsv `spec` value) its unqualified citations mean.
CRATES="
crates/gamut-icc icc.1-2022-05
crates/gamut-cmm icc.1-2022-05
crates/gamut-png png-3
"
INDEXES="$root/references/icc/clauses.tsv $root/references/png/clauses.tsv"

# Every pair here is open work for issue #879, which qualifies or corrects each citation and empties
# this list. Until then the guard reports nothing it already knows and everything new. One is a
# wrong citation (`IHDR (§11.2.2)`: §11.2.2 is PLTE); the rest are C2PA 2.4 clauses cited inside
# gamut-png with no designator on the citing line, which therefore reads as a PNG clause that does
# not exist.
KNOWN="
crates/gamut-png/src/backend.rs	11.2.2
crates/gamut-png/STATUS.md	A.3.2
crates/gamut-png/STATUS.md	18.5.4
crates/gamut-png/src/ancillary.rs	A.3.2
crates/gamut-png/src/chunk.rs	18.5
crates/gamut-png/src/chunk.rs	A.3.2
crates/gamut-png/src/chunk.rs	A.3.6
crates/gamut-png/src/decoder.rs	A.3.2
crates/gamut-png/src/deconstruct.rs	A.3.2
crates/gamut-png/src/encoder.rs	18.5.4
crates/gamut-png/src/lib.rs	18.5.4
crates/gamut-png/tests/c2pa.rs	A.3.2
crates/gamut-png/tests/c2pa.rs	18.5.4
"

for t in $INDEXES; do
    test -f "$t" || {
        echo "check-citations: missing clause index $t (run 'mise run gen-spec-clauses')"
        exit 1
    }
done

# `default-spec <TAB> path` for every file to read.
if [ "$#" -gt 0 ]; then
    spec="$1"
    shift
    [ "$#" -gt 0 ] || {
        echo "usage: check-citations/run.sh [SPEC FILE...]"
        exit 2
    }
    list="$(for f in "$@"; do printf '%s\t%s\n' "$spec" "$f"; done)"
    known=""
else
    cd "$root" || exit 1
    list="$(
        printf '%s\n' "$CRATES" | while read -r dir spec; do
            [ -n "$dir" ] || continue
            git ls-files -- "$dir" | grep -E '\.(rs|md|toml)$' | while IFS= read -r f; do
                printf '%s\t%s\n' "$spec" "$f"
            done
        done
    )"
    known="$KNOWN"
fi

# `CHECK_CITATIONS_AWK` picks the interpreter, unquoted so `CHECK_CITATIONS_AWK='gawk --posix'`
# works -- the portability claim is re-runnable rather than asserted.
# KNOWN reaches awk `;`-joined: POSIX awk refuses a newline inside a `-v` value.
known="$(printf '%s' "$known" | tr '\n' ';')"
# SC2086: INDEXES is a fixed list of paths without spaces, and the awk command word-splits on
# purpose. SC2016: the backticks below are awk string text, not command substitution.
# shellcheck disable=SC2086,SC2016
printf '%s\n' "$list" | LC_ALL=C ${CHECK_CITATIONS_AWK:-awk} -F'\t' -v known="$known" '
# Byte-oriented under LC_ALL=C: "§" is \302\247, an en dash \342\200\223, an em dash \342\200\224.
function is_ident(w) {
    return length(w) >= 4 && w ~ /^[A-Za-z][A-Za-z0-9]*$/ && substr(w, 2) ~ /[A-Z0-9]/
}
function words_of(s,    t) {
    t = s
    gsub(/[^A-Za-z0-9]/, " ", t)
    return " " t " "
}
# The spec a designator word names: an index id, "FOREIGN", or "" (not a designator).
function designator(w) {
    sub(/^[(`*\[]+/, "", w)
    if (w ~ /^ICC\.1:2001/) return "icc.1-2001-04"
    if (w == "ICC.1" || w ~ /^ICC\.1:2022/) return "icc.1-2022-05"
    if (w == "PNG") return "png-3"
    if (w ~ /[.,;:]$/) return ""
    if (w ~ /[0-9]/ || w ~ /\)$/) return "FOREIGN"
    if (w ~ /^(C2PA|RFC|ISO|IEC|ITU|ITU-T|TIFF|EXIF|Exif|JFIF|DNG|XMP|IPTC|CIPA|BCP|JPEG|HEIF|ISOBMFF|CIE)$/) return "FOREIGN"
    return ""
}
function last_word(s,    n, parts) {
    sub(/[ \t]+$/, "", s)
    n = split(s, parts, /[ \t]+/)
    return n ? parts[n] : ""
}
# Comment markup and nothing else: what precedes a citation that opens its line.
function only_markup(s) {
    return s ~ /^[ \t]*((\/\/[\/!]?|\*|#+|-|>|\|)[ \t]*)*$/
}
function report(file, lineno, num, spec, msg) {
    if ((file SUBSEP num) in known_pair) {
        known_hit[file SUBSEP num] = 1
        known_count++
        return
    }
    printf "%s:%d: \302\247%s (%s): %s\n", file, lineno, num, spec, msg
    errors++
}
function flush_group(file, lineno) {
    if (g_name != "" && !g_ok && ((g_spec, g_first) in title)) {
        report(file, lineno, g_first, g_spec, "cited for `" g_name "`, but " g_first " is \"" title[g_spec, g_first] "\"; `" g_name "` heads " heads[g_spec, g_name])
    }
    g_name = ""
}
function check_line(file, lineno, line, prev, dspec,    rest, off, s, num, after, pre, gap, spec, w, q, name, raw, t, nend) {
    off = 0
    nend = 0
    spec = ""
    rest = line
    while (match(rest, /\302\247 ?([A-Z](\.[0-9]+)*|[0-9]+(\.[0-9]+)*)/)) {
        s = off + RSTART
        raw = substr(rest, RSTART, RLENGTH)
        num = raw
        sub(/^\302\247 ?/, "", num)
        after = substr(line, s + RLENGTH, 1)
        off = s + RLENGTH - 1
        rest = substr(line, off + 1)
        if (after ~ /[A-Za-z0-9]/) continue
        pre = substr(line, 1, s - 1)
        gap = (nend > 0) ? substr(line, nend + 1, s - nend - 1) : "x"
        nend = off
        name = ""
        if (gap !~ /^([ ,;\/-]|\342\200\223|\342\200\224)*((and|or|to|through)[ ]+)?$/) {
            flush_group(file, lineno)
            q = ""
            t = pre
            if (only_markup(t)) {
                # A wrapped list (`C2PA 2.4 §A.3.2,` / `§18.5.4`) continues the last citation of
                # the line before; a wrapped designator (`(ICC.1:2022` / `§7.2.6`) qualifies this.
                q = (tail_spec != "") ? tail_spec : designator(last_word(prev))
            } else if (t ~ /[^ ] $/) {
                q = designator(last_word(t))
                if (q != "") {
                    # Drop the designator but keep a parenthesis it opened: `iCCP (PNG §`.
                    t = substr(t, 1, length(t) - 1)
                    sub(/[^ \t(]+$/, "", t)
                }
            }
            if (q == "" && pre ~ /(C2PA|Adobe RGB|H\.273|RFC [0-9])/) q = "FOREIGN"
            spec = (q != "") ? q : dspec
            if (t ~ /[^ (] ?\( ?$/) {
                sub(/ ?\( ?$/, "", t)
                w = last_word(t)
                gsub(/`/, "", w)
                name = w
            }
            # The attached name opens a group: it must head one of the clauses the group cites
            # (`curveType` and `parametricCurveType` (ICC.1:2022 §10.6, §10.18)).
            if (spec != "FOREIGN" && is_ident(name) && ((spec, name) in heads)) {
                g_name = name
                g_spec = spec
                g_first = num
                g_ok = 0
            }
        }
        total++
        last_spec = spec
        if (spec == "FOREIGN") { foreign++; continue }
        checked++
        if (!((spec, num) in title)) {
            report(file, lineno, num, spec, "no clause heading numbered " num " in " spec)
            continue
        }
        if (g_name != "" && index(words_of(title[spec, num]), " " g_name " ")) g_ok = 1
    }
    flush_group(file, lineno)
    # The spec a list left open at the end of this line hands to the next line, if any.
    t = substr(line, nend + 1)
    tail_spec = (nend > 0 && t ~ /^([ ,;\/-]|\342\200\223|\342\200\224)*((and|or|to|through)[ ]*)?$/) ? last_spec : ""
}
BEGIN {
    n = split(known, kl, ";")
    for (i = 1; i <= n; i++) {
        if (split(kl[i], kf, "\t") == 2) known_pair[kf[1] SUBSEP kf[2]] = kl[i]
    }
}
FILENAME ~ /clauses\.tsv$/ && FNR == 1 { next } # header row
FILENAME ~ /clauses\.tsv$/ {
    title[$1, $2] = $3
    n = split(words_of($3), ws, " ")
    for (k = 1; k <= n; k++) {
        if (!is_ident(ws[k])) continue
        if (!(($1, ws[k]) in heads))
            heads[$1, ws[k]] = $2
        else if (index(" " heads[$1, ws[k]] " ", " " $2 " ") == 0)
            heads[$1, ws[k]] = heads[$1, ws[k]] " " $2
    }
    next
}
$2 != "" {
    lineno = 0
    prev = ""
    tail_spec = ""
    while ((getline line < $2) > 0) {
        lineno++
        if (index(line, "\302\247")) check_line($2, lineno, line, prev, $1)
        else tail_spec = ""
        prev = line
    }
    close($2)
    files++
}
END {
    for (p in known_pair) {
        if (!(p in known_hit)) {
            split(p, kf, SUBSEP)
            printf "%s: \302\247%s is in KNOWN but no longer reported; delete its KNOWN line\n", kf[1], kf[2]
            errors++
        }
    }
    printf "check-citations: %d citations in %d files: %d checked against an index, %d into a specification with none here", total, files, checked, foreign
    if (known_count) printf "; %d known finding(s) in KNOWN", known_count
    printf "\n"
    if (errors) {
        printf "check-citations: %d problem(s): see references/*/clauses.tsv for the clause headings\n", errors
        exit 1
    }
}
' $INDEXES -
