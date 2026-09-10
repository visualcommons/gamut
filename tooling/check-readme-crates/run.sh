#!/usr/bin/env bash
# The README's "## Crates" table is hand-maintained prose, and nothing read it. By issue #425 it
# had drifted far enough that eighteen rows needed correcting -- five shipped crates were still
# described as unstarted scaffolding citing a closed issue -- and four crates (gamut-codec-abi,
# gamut-dng, gamut-jpeg, gamut-tonemap) had no row at all.
#
# This guard checks what is mechanical about the table -- its *membership* and the *shape* of a
# row -- and nothing about the prose in it:
#
#   * every workspace crate has a row, so a new crate cannot be added without documenting it;
#   * every row names a crate that still exists, so a renamed or deleted crate cannot be left
#     behind as a phantom row;
#   * no crate is listed twice, so the table stays a bijection rather than a set;
#   * every crate row is a well-formed three-cell row with a non-empty Purpose and Status, so a
#     row cannot be reduced to a bare crate name and still satisfy membership.
#
# What is deliberately NOT checked, and why:
#   * The *wording* of the Purpose and Status cells. They are prose a human maintains, and their
#     authority is the crate's own lib.rs, Cargo.toml and STATUS.md. A text gate over them would
#     fossilise a particular phrasing, and a generated table would move prose a human writes into
#     a generator -- so staleness of a row's *text* stays a review concern, not a lint. What a
#     machine can settle is membership and shape, and it settles both.
#   * The version. `mise run versions` already reports it, and the README deliberately states
#     what each crate *is* rather than pinning a number that release-plz bumps.
set -euo pipefail

# With no argument, check the repository's own README from wherever the task was invoked. With
# one, check that file as given -- so a caller can point the guard at a candidate README without
# the working directory changing what "README.md" means.
if [ "$#" -eq 0 ]; then
    root="$(git rev-parse --show-toplevel)" || {
        echo "check-readme-crates: not inside a git repository, and no README path was given"
        exit 1
    }
    cd "$root" || exit 1
    readme="README.md"
else
    readme="$1"
fi

test -f "$readme" || {
    echo "check-readme-crates: no such file: $readme"
    exit 1
}

# The rows of the "## Crates" table only. Bounded to that section so the README's other tables
# (the `mise run ...` command table) can never be mistaken for a crate row.
rows="$(
    awk '/^## Crates$/ { in_section = 1; next } /^## / { in_section = 0 } in_section' "$readme" |
        sed -nE 's/^\| *`([a-z0-9-]+)` *\|.*/\1/p'
)"

test -n "$rows" || {
    echo "check-readme-crates: found no crate rows under '## Crates' in $readme"
    exit 1
}

fail=0

# Shape, as distinct from prose. Membership reads a row's first cell only, so a row stripped of
# its Purpose and Status -- `| `gamut-core` |` -- names a live crate and passes membership while
# documenting nothing. An escaped pipe is swapped for a sentinel first, so a cell holding a
# literal `\|` stays content rather than becoming an extra column.
malformed="$(
    awk '
        /^## Crates$/ { in_section = 1; next }
        /^## /        { in_section = 0 }
        !in_section   { next }
        /^\| *`[a-z0-9-]+` *\|/ {
            row = $0
            gsub(/\\\|/, "\001", row)
            n = split(row, cell, "|")
            name = cell[2]
            gsub(/[` ]/, "", name)
            if (n != 5) {
                print name " has " (n - 2) " cell(s); a crate row is Crate | Purpose | Status"
                next
            }
            purpose = cell[3]
            status = cell[4]
            gsub(/^[ \001]+|[ \001]+$/, "", purpose)
            gsub(/^[ \001]+|[ \001]+$/, "", status)
            if (purpose == "") { print name " has an empty Purpose cell" }
            if (status == "")  { print name " has an empty Status cell" }
        }
    ' "$readme"
)"
if [ -n "$malformed" ]; then
    fail=1
    echo "check-readme-crates: malformed crate rows in the $readme crates table:"
    echo "$malformed" | sed 's/^/  /'
fi

# LC_ALL=C throughout: `comm` exits non-zero on input it considers unsorted, and jq's `sort_by`
# below orders by codepoint. Collating both sides the same way keeps them comparable under any
# ambient locale.
readme_crates="$(echo "$rows" | LC_ALL=C sort -u)"

duplicates="$(echo "$rows" | LC_ALL=C sort | uniq -d)"
if [ -n "$duplicates" ]; then
    fail=1
    echo "check-readme-crates: crates listed more than once in the $readme crates table:"
    echo "$duplicates" | sed 's/^/  /'
fi

# The same source of truth `mise run versions` reads: --no-deps lists workspace members only.
workspace_crates="$(
    cargo metadata --no-deps --format-version 1 | jq -r '.packages | sort_by(.name)[] | .name'
)" || {
    echo "check-readme-crates: could not read the workspace crate list from cargo metadata"
    exit 1
}

# `comm` failing is a bug in this script (unsorted input), not a README defect, so it is reported
# as itself rather than being swallowed by `set -e` into a bare exit. The diagnostic goes to
# stderr because this runs inside a command substitution -- on stdout it would be captured as if
# it were a crate name instead of shown.
compare() {
    LC_ALL=C comm "$1" <(echo "$workspace_crates") <(echo "$readme_crates") || {
        echo "check-readme-crates: internal error comparing crate lists (comm $1)" >&2
        exit 1
    }
}

missing="$(compare -23)"
if [ -n "$missing" ]; then
    fail=1
    echo "check-readme-crates: workspace crates with no row in the $readme crates table:"
    echo "$missing" | sed 's/^/  /'
    echo "  add a row (Crate | Purpose | Status), taking the status from the crate's STATUS.md."
fi

phantom="$(compare -13)"
if [ -n "$phantom" ]; then
    fail=1
    echo "check-readme-crates: $readme crates table names crates that are not workspace members:"
    echo "$phantom" | sed 's/^/  /'
    echo "  drop the row, or fix the crate name it misspells."
fi

if [ "$fail" -eq 0 ]; then
    echo "README crates table lists every workspace crate ($(echo "$workspace_crates" | wc -l) crates)"
fi

exit "$fail"
