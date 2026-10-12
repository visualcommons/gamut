#!/usr/bin/env bash
# AGENTS.md's "Dependency edges" list is the repository's own map of the workspace graph, and the
# issue-ordering rule ("work moves up the dependency graph") is read off it. Nothing read it: by
# issue #595 eleven of its crates disagreed with `cargo metadata` -- two claimed a dependency on
# `gamut-core` they do not have, three omitted `gamut-codec-abi`, and `gamut-dng -> gamut-jxl`, an
# edge between two format crates, was not written anywhere. This guard holds the list to the graph.
#
# THE GRAMMAR it reads is stated once, in AGENTS.md itself, directly under the list's lead-in line:
#
#   * a bullet opens with the crates it describes, each in bold (`**gamut-x**`), optionally followed
#     by one parenthesis, and separated by ` / `. Bold text later in the bullet is prose;
#   * the bullet holds exactly one `←`, and the clause after it lists the edges of every crate the
#     bullet opens with. Items are comma-separated; the clause ends at the first `.` or `;` outside
#     parentheses and code spans, or at the end of the bullet;
#   * an item is a workspace crate written without its `gamut-` prefix (`gamut` is the umbrella), an
#     external crate in backticks, or `nothing` alone. A parenthesis after the name that opens with
#     `optional` marks an optional dependency, one that opens with `build` a build dependency, and
#     any other parenthesis is a note the guard does not read.
#
# WHAT IS CHECKED, against `cargo metadata --no-deps` (nothing is built or fetched):
#
#   * every workspace member opens exactly one bullet, and every bullet opens with members only;
#   * each crate's listed edges equal its normal and build dependencies, external ones included, so
#     a new dependency cannot land without the map saying so; and each edge's kind (normal,
#     optional, build) matches the manifest.
#
# WHAT IS NOT CHECKED: the prose around the clauses, a dependency's target `cfg` (written as a note
# where it matters), feature names inside an `optional` note, and dev-dependencies, which are not
# part of the graph the ordering rule reads (`check-release-deps` owns dev-edge topology).
#
# Usage:
#     tooling/check-agents-edges/run.sh              # the repository's AGENTS.md
#     tooling/check-agents-edges/run.sh PATH         # another file, read with the same grammar
set -euo pipefail

if [ "$#" -eq 0 ]; then
    root="$(git rev-parse --show-toplevel)" || {
        echo "check-agents-edges: not inside a git repository, and no path was given"
        exit 1
    }
    cd "$root" || exit 1
    set -- AGENTS.md
fi
doc="$1"
test -f "$doc" || {
    echo "check-agents-edges: no such file: $doc"
    exit 1
}

# One pass over the section. Emits `CRATE <name>`, `EDGE <crate> <dep> <kind>` and `BAD <message>`,
# tab-separated, so membership and edges are read from the same parse of the same bullets.
documented="$(LC_ALL=C awk '
function flush(    b, heads, nheads, rest, n, clause, i, c, depth, code, item, items, nitems, done) {
    if (bullet == "") return
    b = bullet
    bullet = ""
    nheads = 0
    while (match(b, /^\*\*[A-Za-z0-9_-]+\*\*/)) {
        heads[++nheads] = substr(b, 3, RLENGTH - 4)
        b = substr(b, RLENGTH + 1)
        if (match(b, /^ \([^)]*\)/)) b = substr(b, RLENGTH + 1)
        if (substr(b, 1, 3) != " / ") break
        b = substr(b, 4)
    }
    if (nheads == 0) {
        printf "BAD\ta bullet does not open with a bold crate name: %s\n", substr(bullet_text, 1, 60)
        return
    }
    for (i = 1; i <= nheads; i++) printf "CRATE\t%s\n", heads[i]
    rest = bullet_text
    n = gsub(/←/, "←", rest)
    if (n != 1) {
        printf "BAD\t%s: the bullet holds %d `←` clauses; it must hold exactly one\n", heads[1], n
        return
    }
    clause = substr(bullet_text, index(bullet_text, "←") + length("←"))
    depth = 0; code = 0; item = ""; nitems = 0; done = 0
    for (i = 1; i <= length(clause) && !done; i++) {
        c = substr(clause, i, 1)
        if (c == "`") code = !code
        else if (!code && c == "(") depth++
        else if (!code && c == ")") depth--
        if (!code && depth == 0 && (c == "," || c == "." || c == ";")) {
            items[++nitems] = item
            item = ""
            if (c != ",") done = 1
            continue
        }
        item = item c
    }
    if (!done) items[++nitems] = item
    for (i = 1; i <= nitems; i++) edge(heads, nheads, items[i], nitems)
}
function edge(heads, nheads, raw, nitems,    name, qual, kind, p, j) {
    gsub(/^ +| +$/, "", raw)
    name = raw
    qual = ""
    p = index(raw, "(")
    if (p > 0) {
        name = substr(raw, 1, p - 1)
        qual = substr(raw, p + 1)
        sub(/\) *$/, "", qual)
        gsub(/ +$/, "", name)
    }
    if (name ~ /^`[A-Za-z0-9_-]+`$/) name = substr(name, 2, length(name) - 2)
    else if (name == "nothing" && qual == "") {
        if (nitems != 1) printf "BAD\t%s: `nothing` must be the only item of its clause\n", heads[1]
        return
    } else if (name ~ /^gamut-/) {
        printf "BAD\t%s: write the edge `%s` without its `gamut-` prefix\n", heads[1], name
        return
    } else if (name == "gamut") name = "gamut"
    else if (name ~ /^[a-z0-9]+(-[a-z0-9]+)*$/) name = "gamut-" name
    else {
        printf "BAD\t%s: unrecognised edge item `%s` (want a prefix-less workspace name, a backticked external crate, or `nothing`)\n", heads[1], raw
        return
    }
    kind = "normal"
    if (qual ~ /^optional/) kind = "optional"
    else if (qual ~ /^build/) kind = "build"
    for (j = 1; j <= nheads; j++) printf "EDGE\t%s\t%s\t%s\n", heads[j], name, kind
}
{ sub(/\r$/, "") }
/^Dependency edges \(/ { insec = 1; next }
insec && /^#/ { flush(); insec = 0; next }
!insec { next }
/^- / { flush(); bullet = substr($0, 3); bullet_text = bullet; next }
/^  [^ ]/ && bullet != "" { line = $0; sub(/^ +/, "", line); bullet = bullet " " line; bullet_text = bullet; next }
{ flush() }
END { flush() }
' "$doc")"

actual="$(cargo metadata --no-deps --format-version 1 | jq -r '
  (.workspace_members | map({key: ., value: true}) | from_entries) as $members
  | .packages[] | select($members[.id]) as $p
  | "CRATE\t\($p.name)",
    ($p.dependencies[]
     | select((.kind // "normal") == "normal" or .kind == "build")
     | "EDGE\t\($p.name)\t\(.name)\t\(if .kind == "build" then "build" elif .optional then "optional" else "normal" end)")
' | sort -u)"

fail=0
report() {
    echo "check-agents-edges: $1"
    fail=1
}

while IFS=$'\t' read -r _ msg; do
    report "$msg"
done < <(printf '%s\n' "$documented" | grep '^BAD' || true)

doc_crates="$(printf '%s\n' "$documented" | awk -F '\t' '$1 == "CRATE" { print $2 }' | sort)"
real_crates="$(printf '%s\n' "$actual" | awk -F '\t' '$1 == "CRATE" { print $2 }' | sort)"

while read -r name; do
    [ -n "$name" ] && report "$name opens more than one bullet; a crate's edges are stated once"
done < <(printf '%s\n' "$doc_crates" | uniq -d)
while read -r name; do
    [ -n "$name" ] && report "$name is a workspace member with no bullet; give it one with a \`←\` clause"
done < <(comm -13 <(printf '%s\n' "$doc_crates" | sort -u) <(printf '%s\n' "$real_crates"))
while read -r name; do
    [ -n "$name" ] && report "$name opens a bullet but is not a workspace member"
done < <(comm -23 <(printf '%s\n' "$doc_crates" | sort -u) <(printf '%s\n' "$real_crates"))

# Edges are compared only for crates both sides know, so a missing or phantom bullet is reported
# once, above, rather than once per edge.
known="$(comm -12 <(printf '%s\n' "$doc_crates" | sort -u) <(printf '%s\n' "$real_crates"))"
edges() { printf '%s\n' "$1" | awk -F '\t' -v known="$known" '
    BEGIN { n = split(known, k, "\n"); for (i = 1; i <= n; i++) ok[k[i]] = 1 }
    $1 == "EDGE" && ($2 in ok) { print $2 "\t" $3 "\t" $4 }' | sort -u; }
doc_edges="$(edges "$documented")"
real_edges="$(edges "$actual")"

while IFS=$'\t' read -r crate dep kind; do
    [ -n "$crate" ] && report "$crate: AGENTS.md lists an edge to $dep ($kind) that cargo metadata does not have"
done < <(comm -23 <(printf '%s\n' "$doc_edges") <(printf '%s\n' "$real_edges"))
while IFS=$'\t' read -r crate dep kind; do
    [ -n "$crate" ] && report "$crate: cargo metadata has an edge to $dep ($kind) that AGENTS.md does not list"
done < <(comm -13 <(printf '%s\n' "$doc_edges") <(printf '%s\n' "$real_edges"))

if [ "$fail" -ne 0 ]; then
    echo "check-agents-edges: AGENTS.md's dependency edges disagree with cargo metadata (grammar: tooling/check-agents-edges/run.sh header)"
    exit 1
fi
echo "check-agents-edges: $(printf '%s\n' "$real_crates" | grep -c .) crates, $(printf '%s\n' "$real_edges" | grep -c .) edges agree with cargo metadata"
