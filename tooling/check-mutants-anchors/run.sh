#!/usr/bin/env bash
# Every `exclude_re` entry in `.cargo/mutants.toml` names one crate-qualified site that still
# carries the operator its comment argues about (issue #1054).
#
# A cargo-mutants exclusion is a regex over the mutant's name, `PATH:LINE:COLUMN: DESCRIPTION`. A
# line-anchored entry stops matching the moment code above it moves, and an exclusion that matches
# nothing fails nowhere: the mutant it argued for silently returns to the survey, and a wrongly
# re-anchored one silently excludes a sibling nobody argued for. Before this check, 29 of 44
# line-anchored entries had drifted that way, and bare function names matched same-named functions
# in other crates. So the anchor is held to the source here, statically -- no cargo, no build.
#
# The two forms an entry may take (anything else is refused):
#
#   1. SITE.  '^crates/<crate>/src/<path>\.rs:L:C: <description>$', or with several sites of one
#      statement as '...:(L:C|L:C): <description>$'. For each site this verifies that
#        * the file exists and has a line L;
#        * an operator description (`replace OP with ...`, `delete OP`, `replace match guard G
#          with ...`, `delete match arm P`, `delete field F from ...`) finds OP / G / P / F
#          starting at column C of that line -- columns count from 1, as cargo-mutants prints them;
#        * a description ending `in FN` (and a whole-body `replace FN -> T with V`) sits inside
#          `fn <last segment of FN>`: the nearest `fn` item at or above line L carries that name;
#        * a whole-body description sits exactly where cargo-mutants names that mutant: L:C is the
#          first token of the body, after the `{` that closes the signature and any comments
#          (issue #1162). Not the signature line -- an entry anchored there satisfies the bullet
#          above and matches nothing, so the hang or equivalence its comment argued for stayed in
#          the survey (`DecTable::walk` was anchored that way).
#   2. FUNCTION.  '^crates/<crate>/src/<path>\.rs:\d+:\d+: .* in FN$' -- a claim about every mutant
#      in one function, which survives movement by construction. This verifies the file declares
#      `fn <last segment of FN>`.
#
# What is deliberately NOT checked: the replacement half of a description (`with >=`), which the
# operator at the site already pins, and whether the argument in the comment is still true -- that
# is the survey's job (`mise run mutants`), not a text check's.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
# The config to check; an argument exists so the check can be pointed at an edited copy.
config=${1:-.cargo/mutants.toml}

# Only the single-quoted lines inside the `exclude_re = [ ... ]` array. Literal strings are
# required so that what is read here is byte-for-byte the regex cargo-mutants compiles.
entries=$(awk -v q="'" '
    /^exclude_re = \[/ { inside = 1; next }
    inside && /^\]/ { inside = 0 }
    inside && /^[[:space:]]*"/ { printf "DOUBLE\t%d\t%s\n", NR, $0; next }
    inside && $0 ~ ("^[[:space:]]*" q) { printf "ENTRY\t%d\t%s\n", NR, $0 }
' "$config")

if [ -z "$entries" ]; then
    echo "check-mutants-anchors: no exclude_re entries found in $config" >&2
    exit 1
fi

# The rest is one awk program: it reads each anchored file once and checks every site in it.
printf '%s\n' "$entries" | awk -F '\t' -v config="$config" -v q="'" '
function fail(msg) {
    printf "  %s:%d: %s\n", config, lineno, msg
    bad++
}
# Undo the regex escaping of a description: `\|` -> `|`, `\(` -> `(`, `\.` -> `.`.
function unescape(s,    out, i, ch) {
    out = ""
    for (i = 1; i <= length(s); i++) {
        ch = substr(s, i, 1)
        if (ch == "\\" && i < length(s)) { i++; ch = substr(s, i, 1) }
        out = out ch
    }
    return out
}
function load(path,    line, n) {
    if (path in loaded) return loaded[path]
    n = 0
    while ((getline line < path) > 0) src[path, ++n] = line
    close(path)
    loaded[path] = n
    return n
}
# The name of the nearest `fn` item at or above line l, or "" if there is none. Sets fn_line and
# fn_col to where its `fn` keyword starts.
function enclosing_fn(path, l,    i, line, rest) {
    for (i = l; i >= 1; i--) {
        line = src[path, i]
        if (line ~ /^[[:space:]]*\/\//) continue
        if (match(line, /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
            rest = substr(line, RSTART, RLENGTH)
            fn_line = i
            fn_col = RSTART + index(rest, "fn") - 1
            sub(/^.*fn[[:space:]]+/, "", rest)
            return rest
        }
    }
    return ""
}
# "L:C" of the first token of the body of the `fn` whose keyword is at line l, column c -- where
# cargo-mutants names a whole-body replacement -- or "" if there is none. The body opens at the
# first `{` outside the parentheses and brackets of the signature; blank space and comments after it
# are skipped, as they carry no token.
function body_start(path, l, c,    n, line, ch, depth, open, j, in_block) {
    n = loaded[path]
    depth = 0
    open = 0
    for (; l <= n; l++) {
        line = src[path, l]
        for (j = c; j <= length(line); j++) {
            ch = substr(line, j, 1)
            if (in_block) {
                if (substr(line, j, 2) == "*/") { in_block = 0; j++ }
                continue
            }
            if (!open) {
                if (ch == "(" || ch == "[") depth++
                else if (ch == ")" || ch == "]") depth--
                else if (ch == "{" && depth == 0) open = 1
                continue
            }
            if (substr(line, j, 2) == "//") break
            if (substr(line, j, 2) == "/*") { in_block = 1; j++; continue }
            if (ch ~ /[[:space:]]/) continue
            return ch == "}" ? "" : l ":" j
        }
        c = 1
    }
    return ""
}
# `BitReader<\x27a>::f64` -> `f64`, `<impl Iterator for NalUnitIter<\x27a>>::next` -> `next`.
function last_segment(fn,    prev) {
    do { prev = fn; gsub(/<[^<>]*>/, "", fn) } while (fn != prev)
    sub(/^.*::/, "", fn)
    return fn
}
function declares_fn(path, name,    n, i) {
    n = load(path)
    for (i = 1; i <= n; i++)
        if (src[path, i] ~ ("(^|[^A-Za-z0-9_])fn[[:space:]]+" name "([^A-Za-z0-9_]|$)")) return 1
    return 0
}
BEGIN { bad = 0; count = 0 }
$1 == "DOUBLE" {
    lineno = $2
    fail("entry is a double-quoted string; write it as a single-quoted literal")
    next
}
{
    lineno = $2
    raw = $3
    count++
    sub("^[[:space:]]*" q, "", raw)
    sub(q ",?[[:space:]]*(#.*)?$", "", raw)

    # ^crates/<crate>/src/<path>:<sites>: <description>$
    if (!match(raw, /^\^crates\/[a-z0-9-]+\/src\/[^:]+:/)) {
        fail("not crate-qualified (want ^crates/<crate>/src/<path>:L:C: ...$): " raw)
        next
    }
    path = unescape(substr(raw, 2, RLENGTH - 2))
    rest = substr(raw, RLENGTH + 1)
    if (substr(rest, length(rest)) != "$") {
        fail("not end-anchored with $: " raw)
        next
    }
    rest = substr(rest, 1, length(rest) - 1)
    split_at = index(rest, ": ")
    if (split_at == 0) { fail("no \": \" between the site and the description: " raw); next }
    sites = substr(rest, 1, split_at - 1)
    desc = unescape(substr(rest, split_at + 2))
    if (load(path) == 0) { fail("no such file (or it is empty): " path); next }

    # The function is whatever follows the last ` in `; no function name contains one.
    fn = ""
    head = desc
    for (i = length(desc) - 3; i > 1; i--) {
        if (substr(desc, i, 4) == " in ") {
            fn = substr(desc, i + 4)
            head = substr(desc, 1, i - 1)
            break
        }
    }

    if (sites == "\\d+:\\d+") {
        # FUNCTION form: a claim about every mutant of one function.
        if (fn == "" && match(desc, /^replace [^ ]+ -> /)) {
            fn = substr(desc, 9)
            sub(/ -> .*$/, "", fn)
        }
        if (fn == "") { fail("an unpinned site needs a function to anchor on: " raw); next }
        if (!declares_fn(path, last_segment(fn)))
            fail(path " declares no fn " last_segment(fn) " (" raw ")")
        next
    }

    # SITE form: L:C or (L:C|L:C|...).
    if (sites ~ /^\(.*\)$/) sites = substr(sites, 2, length(sites) - 2)
    nsites = split(sites, site, "|")

    # The token that must start at column C, or "" for a whole-body replacement.
    want = ""
    body = 0
    if (match(head, /^replace match guard /)) {
        want = substr(head, RLENGTH + 1)
        sub(/ with (true|false)$/, "", want)
        sub(/ .*$/, "", want)
    } else if (match(head, /^delete match arm /)) {
        want = substr(head, RLENGTH + 1)
        match(want, /^[A-Za-z0-9_:]+/)
        want = substr(want, 1, RLENGTH)
    } else if (match(head, /^delete field [A-Za-z0-9_]+ /)) {
        want = substr(head, 14, RLENGTH - 14)
    } else if (match(head, /^delete [^ ]+$/)) {
        want = substr(head, 8)
    } else if (match(head, /^replace [-+*\/%&|^!<>=]+ with /)) {
        want = substr(head, 9)
        sub(/ with .*$/, "", want)
    } else if (match(head, /^replace [^ ].* with /)) {
        # Whole-body: `replace FN -> T with V` or `replace <impl Drop for X>::drop with ()`.
        body = 1
        fn = substr(head, 9)
        if (index(fn, " -> ")) sub(/ -> .*$/, "", fn); else sub(/ with .*$/, "", fn)
    } else {
        fail("description names no operator this check understands: " raw)
        next
    }

    for (s = 1; s <= nsites; s++) {
        if (site[s] !~ /^[0-9]+:[0-9]+$/) { fail("site " site[s] " is not LINE:COLUMN: " raw); continue }
        split(site[s], lc, ":")
        l = lc[1] + 0; c = lc[2] + 0
        if (l > loaded[path]) { fail(path " has no line " l " (" raw ")"); continue }
        if (!body && substr(src[path, l], c, length(want)) != want) {
            fail(sprintf("%s:%d:%d carries \"%s\", not \"%s\"", path, l, c, \
                substr(src[path, l], c, length(want) + 8), want))
            continue
        }
        if (fn != "") {
            got = enclosing_fn(path, l)
            if (got != last_segment(fn)) {
                fail(sprintf("%s:%d sits in fn %s, not %s", path, l, got == "" ? "(none)" : got, fn))
                continue
            }
            if (body && (start = body_start(path, fn_line, fn_col)) != l ":" c)
                fail(sprintf("%s:%d:%d is not where %s names its whole-body mutant: its body starts at %s", \
                    path, l, c, fn, start == "" ? "(no token)" : start))
        }
    }
}
END {
    if (bad) {
        printf "check-mutants-anchors: %d problem(s) across %d exclude_re entries in %s.\n", bad, count, config
        print "Re-anchor each entry on its mutant as `cargo mutants --list` names it, and see docs/mutation-testing.md."
        exit 1
    }
    printf "check-mutants-anchors: %d exclude_re entries, every anchor holds.\n", count
}
'
