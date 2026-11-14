#!/usr/bin/env bash
# No source file under `crates/` grows past 1011 lines unless it is on a visible baseline that may
# only shrink (issue #1096, de-slop finding D33).
#
# Population: the git-tracked `*.rs` and `*.toml` files under `crates/` -- the 701 files the D33
# survey measured (tokei over `crates/`). A file's length is its total line count: code, comments
# and blanks alike, with an unterminated last line counted as a line.
#
# Threshold: 1011, the p90 of that survey. Lower candidates (p50 230, 564) were rejected because
# they would force splitting the inline `#[cfg(test)] mod tests` modules AGENTS.md mandates beside
# the code they test.
#
# The baseline, `tooling/check-file-length/baseline.txt`, lists PATHS, not a count: every file
# that was already over the threshold when the gate landed, after the module-split issues merged.
# One repository-relative path per line, sorted bytewise, no duplicates; `#` lines and blank lines
# are ignored. The check fails when
#   1. a tracked file over the threshold is not listed -- split it, do not list it;
#   2. a listed path is at or under the threshold, or no longer a tracked file -- delete its line,
#      so the baseline shrinks the moment a split lands and cannot be quietly reused;
#   3. the baseline is unsorted or lists a path twice;
#   4. the baseline lists a path the base revision's baseline does not -- the shrink-only rule.
#      Rules 1 and 2 alone would accept a new long file added to the list. A renamed long file is
#      a new path and is refused too: move it by splitting it.
#
# Rule 4 compares against the baseline at `git merge-base HEAD BASE`, where BASE is the first
# argument, else `origin/HEAD`, else `origin/master`. A BASE given as the argument must resolve, or
# the check fails; a defaulted one that does not resolve skips rule 4 with a notice (a fresh clone
# with no remote). Where the base revision has no baseline yet, rule 4 is skipped with a notice.
# Rules 1-3 always run. CI passes the pull request's target branch explicitly.
#
# Pure text, no cargo, sub-second.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

threshold=1011
baseline=tooling/check-file-length/baseline.txt

fail=0
problem() {
    printf 'check-file-length: %s\n' "$1"
    fail=1
}

# The baseline's entries, comments and blanks dropped.
entries() {
    awk '!/^[[:space:]]*(#|$)/' "$@"
}

if [ ! -f "$baseline" ]; then
    echo "check-file-length: $baseline does not exist" >&2
    exit 1
fi

listed=$(entries "$baseline")

# Rule 3: sorted bytewise, unique.
if [ "$listed" != "$(printf '%s\n' "$listed" | LC_ALL=C sort -u | awk 'NF')" ]; then
    problem "$baseline must be sorted bytewise (LC_ALL=C sort -u) with no duplicate paths"
fi

# `LENGTH<TAB>PATH` for every tracked file in the population. awk counts records, so a final line
# without a newline still counts; an empty file has no records and is simply absent (0 lines).
lengths=$(git ls-files -z -- 'crates/**.rs' 'crates/**.toml' \
    | xargs -0 awk '{ n[FILENAME]++ } END { for (f in n) printf "%d\t%s\n", n[f], f }')

# Rules 1 and 2, one pass: the baseline file first, then the lengths.
report=$(printf '%s\n' "$lengths" | awk -F '\t' -v limit="$threshold" -v list="$baseline" '
    FNR == NR { if ($0 !~ /^[[:space:]]*(#|$)/) listed[$0] = 1; next }
    {
        len[$2] = $1
        if ($1 > limit && !($2 in listed))
            printf "%s has %d lines (limit %d) and is not in %s: split it by concern\n", $2, $1, limit, list
    }
    END {
        for (p in listed) {
            if (!(p in len))
                printf "%s is listed in %s but is not a tracked crates/ .rs or .toml file: delete its line\n", p, list
            else if (len[p] <= limit)
                printf "%s is listed in %s but has %d lines (limit %d): delete its line\n", p, list, len[p], limit
        }
    }
' "$baseline" -)
if [ -n "$report" ]; then
    while IFS= read -r line; do problem "$line"; done <<<"$(printf '%s\n' "$report" | LC_ALL=C sort)"
fi

# Rule 4: shrink-only against the base revision's baseline.
base=${1:-}
if [ -z "$base" ]; then
    if git rev-parse --verify --quiet origin/HEAD >/dev/null; then
        base=origin/HEAD
    else
        base=origin/master
    fi
fi
if ! merge_base=$(git merge-base HEAD "$base" 2>/dev/null); then
    # A base the caller named must resolve: CI names one, and a skipped rule there is a gate off.
    if [ -n "${1:-}" ]; then
        problem "base ref '$base' does not resolve, so the shrink-only rule cannot be checked"
    else
        echo "check-file-length: notice: '$base' does not resolve; the shrink-only rule was not checked"
    fi
elif ! base_listed=$(git show "$merge_base:$baseline" 2>/dev/null); then
    echo "check-file-length: notice: $baseline does not exist at $merge_base; the shrink-only rule was not checked"
else
    added=$(LC_ALL=C comm -13 \
        <(printf '%s\n' "$base_listed" | entries | LC_ALL=C sort -u) \
        <(printf '%s\n' "$listed" | LC_ALL=C sort -u | awk 'NF'))
    if [ -n "$added" ]; then
        while IFS= read -r p; do
            problem "$p is in $baseline but not in the baseline at $base ($merge_base): the baseline may only shrink, so split the file instead"
        done <<<"$added"
    fi
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "check-file-length: every tracked crates/ .rs/.toml file is at most $threshold lines or on the shrink-only baseline ($(printf '%s\n' "$listed" | awk 'NF' | wc -l | tr -d ' ') paths)"
