#!/usr/bin/env bash
# The README's "## Crates" table is hand-maintained prose, and nothing read it. By issue #425 it
# had drifted far enough that twenty-three of its twenty-eight rows needed correcting -- five
# shipped crates were still described as unstarted scaffolding citing a closed issue -- and four
# crates (gamut-codec-abi, gamut-dng, gamut-jpeg, gamut-tonemap) had no row at all.
#
# This guard checks what is mechanical about the table -- its *membership*, the *shape* of a row,
# and that the table is a table -- and nothing about the prose in it:
#
#   * every workspace crate has a row, so a new crate cannot be added without documenting it;
#   * every row names a crate that still exists, so a renamed or deleted crate cannot be left
#     behind as a phantom row. A row is recognised by a backticked cargo package name, so the
#     character set is cargo's own -- letters, digits, `-` and `_`, in either case -- and a
#     phantom row cannot hide behind a capital or an underscore;
#   * no crate is listed twice, so the table stays a bijection rather than a set;
#   * every crate row is a three-cell row whose Purpose and Status each *render as something*:
#     a cell holding one non-breaking space, one tab, or one empty HTML element is empty, because
#     that is what a reader sees;
#   * a row inside an HTML comment or a fenced code block is not a row. Both render as something
#     other than a table cell, so a crate documented only there is documented nowhere, and the
#     membership check must see it as missing rather than as present;
#   * the crate rows stand under a `| --- | --- | --- |` delimiter row. The delimiter is what
#     makes the lines around it a table at all: delete it and every row renders as a paragraph of
#     literal pipes while each row's bytes stay intact, which is a table that documents nothing.
#
# CRLF input is accepted: a trailing carriage return is removed from every line before anything is
# matched, so a CRLF README does not silently lose its heading and report itself as rowless.
#
# What is deliberately NOT checked, and why:
#   * The *wording* of the Purpose and Status cells. They are prose a human maintains, and their
#     authority is **the crate's own source** -- the code that ships. No single file outranks it:
#     `STATUS.md` can list a shipped module as deferred (issue #545) and a module doc can defer a
#     capability the crate's own `EncodeImage` impls already provide (issue #560) -- which is why
#     the `gamut-avif` row states 8/10/12-bit encode against a doc comment that still calls
#     10/12-bit deferred. `lib.rs`, `Cargo.toml` and `STATUS.md` are where to look first, in that
#     order, but each is a summary and a row must be true of the crate, not faithful to a file.
#     A text gate over the cells would fossilise a particular phrasing, and a generated table
#     would move prose a human writes into a generator -- so staleness of a row's *text* stays a
#     review concern, not a lint.
#   * The header row's own text. The delimiter above is asserted; what the three column headings
#     are called is prose like any other cell.
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

# One pass over the file emits every stream -- `NAME` for membership, `BAD` for shape, `DELIM` for
# the table delimiter -- so the checks can never disagree about which lines are crate rows.
# Bounded to the "## Crates" section so the README's other tables (the `mise run ...` command
# table) can never be mistaken for a crate row.
#
# LC_ALL=C makes every regexp below byte-wise, which is what the octal escapes assume; the awk
# is POSIX (no gensub, no interval expressions, dynamic regexps built as strings) so it behaves
# the same under mawk, which is what `awk` is on the CI runner.
scan="$(
    LC_ALL=C awk '
        BEGIN {
            # An escaped pipe is content, not a column separator, so it is swapped for a control
            # byte before the split and swapped back before the cell is judged.
            SENTINEL = "\001"
            CR = sprintf("%c", 13)
            # ASCII space and every C0/DEL control byte.
            ASCII_BLANK = "[ \001-\037\177]"
            # The UTF-8 encodings of the Unicode blanks a Markdown renderer shows as nothing:
            # U+00A0, U+1680, U+2000-U+200D, U+2028, U+2029, U+202F, U+205F, U+2060, U+3000 and
            # U+FEFF. Without this fold a single non-breaking space reconstructs exactly the
            # degenerate row the shape check exists to reject.
            UNI_BLANK = "\302\240|\341\232\200|\342\200[\200-\215\250\251\257]"
            UNI_BLANK = UNI_BLANK "|\342\201[\237\240]|\343\200\200|\357\273\277"
        }

        # Returns the part of `line` that is outside an HTML comment, carrying `in_comment`
        # across lines so a multi-line comment hides every line it spans.
        function strip_comments(line,   out, p) {
            out = ""
            while (length(line) > 0) {
                if (in_comment) {
                    p = index(line, "-->")
                    if (p == 0) { return out }
                    in_comment = 0
                    line = substr(line, p + 3)
                } else {
                    p = index(line, "<!--")
                    if (p == 0) { return out line }
                    out = out substr(line, 1, p - 1)
                    in_comment = 1
                    line = substr(line, p + 4)
                }
            }
            return out
        }

        # A closing fence: the opening fence character, repeated at least as many times, and
        # nothing else on the line.
        function is_fence_close(line,   m, i) {
            m = line
            sub(/^ */, "", m)
            sub(/[ \t]*$/, "", m)
            if (length(m) < fence_len) { return 0 }
            for (i = 1; i <= length(m); i++) {
                if (substr(m, i, 1) != fence_char) { return 0 }
            }
            return 1
        }

        # A GFM delimiter row for a three-column table: pipe-separated cells that are nothing but
        # dashes, with an optional alignment colon at either end.
        function is_delim(line,   t, k, c, i, cells) {
            t = line
            sub(/^ */, "", t)
            sub(/[ \t]*$/, "", t)
            if (substr(t, 1, 1) != "|") { return 0 }
            if (substr(t, length(t), 1) == "|") { t = substr(t, 1, length(t) - 1) }
            k = split(t, c, "|")
            cells = 0
            for (i = 2; i <= k; i++) {
                if (c[i] !~ /^ *:?-+:? *$/) { return 0 }
                cells++
            }
            return (cells == 3)
        }

        # Empty means "renders as nothing": nothing survives once HTML tags, the Unicode blanks a
        # renderer collapses, and the ASCII blanks and control bytes are taken out. A cell holding
        # only <span></span> or a lone <br/> is therefore empty, because that is what a reader sees.
        function is_blank(cell,   c) {
            c = cell
            gsub(/<[^<>]*>/, "", c)
            gsub(UNI_BLANK, " ", c)
            gsub(ASCII_BLANK, "", c)
            return c == ""
        }

        {
            line = $0
            if (substr(line, length(line), 1) == CR) {
                line = substr(line, 1, length(line) - 1)
            }

            # Inside a fenced block nothing else is syntax -- not a heading, not a comment, not a
            # row -- until the fence closes.
            if (in_fence) {
                if (is_fence_close(line)) { in_fence = 0 }
                next
            }

            line = strip_comments(line)

            if (match(line, /^ *(```+|~~~+)/)) {
                marker = substr(line, RSTART, RLENGTH)
                sub(/^ +/, "", marker)
                fence_char = substr(marker, 1, 1)
                fence_len = length(marker)
                in_fence = 1
                next
            }

            if (line ~ /^## Crates$/) { in_section = 1; next }
            if (line ~ /^## /)        { in_section = 0; next }
            if (!in_section)          { next }

            # Only a delimiter standing ABOVE the first crate row can be this table delimiter,
            # so it is recorded before any row is seen and reported once at END.
            if (nrows == 0 && is_delim(line)) { delim = 1; next }

            if (line !~ /^\| *`[A-Za-z0-9_-]+` *\|/) { next }

            nrows++
            row = line
            gsub(/\\\|/, SENTINEL, row)
            n = split(row, cell, "|")
            name = cell[2]
            gsub(/[` ]/, "", name)
            print "NAME\t" name

            if (n != 5) {
                print "BAD\t" name " has " (n - 2) " cell(s); a crate row is Crate | Purpose | Status"
                next
            }
            purpose = cell[3]
            status = cell[4]
            gsub(SENTINEL, "|", purpose)
            gsub(SENTINEL, "|", status)
            if (is_blank(purpose)) { print "BAD\t" name " has an empty Purpose cell" }
            if (is_blank(status))  { print "BAD\t" name " has an empty Status cell" }
        }

        END { if (delim) { print "DELIM\tok" } }
    ' "$readme"
)"

rows="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "NAME" { print $2 }')"
malformed="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "BAD" { print $2 }')"
delimiter="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "DELIM" { print $2 }')"

test -n "$rows" || {
    echo "check-readme-crates: found no crate rows under '## Crates' in $readme"
    echo "  either the '## Crates' heading is missing or is not exactly that, or every row under"
    echo "  it is hidden: a row inside an HTML comment or a fenced code block does not count."
    exit 1
}

fail=0

if [ -n "$malformed" ]; then
    fail=1
    echo "check-readme-crates: malformed crate rows in the $readme crates table:"
    echo "$malformed" | sed 's/^/  /'
fi

if [ -z "$delimiter" ]; then
    fail=1
    echo "check-readme-crates: the $readme crates table has no '| --- | --- | --- |' delimiter"
    echo "  row above its first crate row. Without it Markdown renders the whole table as a"
    echo "  paragraph of literal pipes, so every row below it documents nothing."
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
    echo "  add a row (Crate | Purpose | Status). Its authority is the crate's own source -- the"
    echo "  code that ships. Read lib.rs, then Cargo.toml, then STATUS.md, but trust none of them"
    echo "  over the crate: each is a summary and any of them can be stale, so the row must be"
    echo "  true of the crate rather than faithful to a file."
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
