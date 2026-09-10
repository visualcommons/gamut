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
#   * the first crate row is *immediately* preceded by a GFM delimiter row, which is itself
#     immediately preceded by a header row with the same number of columns, and neither is
#     indented four spaces or more. All three parts are what make the lines around them a table at
#     all: remove any one and every row renders as a paragraph of literal pipes while each row's
#     bytes stay intact, which is a table that documents nothing. Adjacency is the load-bearing
#     word -- "a delimiter exists somewhere above" is satisfied by a blank line after it, by an
#     unrelated table earlier in the section, and by a header that does not agree with it, each of
#     which renders as no table.
#
# CRLF input is accepted: a trailing carriage return is removed from every line before anything is
# matched, so a CRLF README does not silently lose its heading and report itself as rowless.
#
# The row pattern is deliberately narrow: a crate row is `| `name` | ... | ... |` with the pipe in
# column 1 and the crate name alone inside a code span. Legal Markdown this does not recognise --
# an omitted leading or trailing pipe, an indented row, a linked or annotated crate cell -- is
# reported as a *missing crate*, and the failure message says so, because widening the pattern is
# how a phantom row gets in. Narrow and loud beats wide and quiet.
#
# One bypass is known and left open, and is stated rather than covered by the word "table": a blank
# line inserted *between* two crate rows splits the table in two, and every row below the blank
# renders as a paragraph while still satisfying membership, shape and the delimiter checks -- only
# the FIRST crate row's header/delimiter context is asserted. Closing it means asserting that the
# crate rows are contiguous, which would reject a legal table that interleaves a non-crate row
# (a `| **Codecs** | | |` separator, say), so it stays a reader's job.
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
#   * The header row's own text. Its *presence* directly above the delimiter and its *column
#     count* are asserted; what the three column headings are called is prose like any other cell.
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
            # The same blanks written as HTML character references, which a Markdown renderer
            # resolves before it shows the cell. This list is the common set, NOT an exhaustive
            # one -- HTML5 names some 2000 references and a decimal or hex form exists for every
            # code point, so an author determined to write an invisible cell can always find a
            # spelling this misses. It closes the spellings a human actually reaches for.
            ENT_BLANK = "&(nbsp|NonBreakingSpace|ensp|emsp|emsp13|emsp14|numsp|puncsp|thinsp"
            ENT_BLANK = ENT_BLANK "|hairsp|VeryThinSpace|MediumSpace|ThickSpace|ThinSpace"
            ENT_BLANK = ENT_BLANK "|zwnj|zwj|ZeroWidthSpace|NegativeThinSpace|NoBreak|lrm|rlm);"
            ENT_BLANK = ENT_BLANK "|&#(160|8194|8195|8196|8197|8199|8200|8201|8202|8203|8204"
            ENT_BLANK = ENT_BLANK "|8205|8232|8233|8239|8287|8288|12288|65279);"
            ENT_BLANK = ENT_BLANK "|&#[xX]0*([Aa]0|1680|200[0-9A-Da-d]|202[8-9Ff]|205[Ff]"
            ENT_BLANK = ENT_BLANK "|2060|3000|[Ff][Ee][Ff][Ff]);"
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

        # Splits a pipe-delimited line into `tcell[2..]` and returns the cell count, or 0 if the
        # line is not a table row at all. Four or more leading spaces is an indented code block,
        # not a table -- so an indented delimiter is no delimiter. An escaped pipe is content, so
        # it is swapped out before the split here too, exactly as the row shape check does.
        function table_cells(line,   t, k, i) {
            t = line
            if (t ~ /^    /) { return 0 }
            sub(/^ */, "", t)
            sub(/[ \t]*$/, "", t)
            if (substr(t, 1, 1) != "|") { return 0 }
            gsub(/\\\|/, SENTINEL, t)
            if (substr(t, length(t), 1) == "|") { t = substr(t, 1, length(t) - 1) }
            k = split(t, tcell, "|")
            for (i = 2; i <= k; i++) { gsub(SENTINEL, "|", tcell[i]) }
            return (k >= 2) ? k - 1 : 0
        }

        # A GFM delimiter row: every cell is nothing but dashes, with an optional alignment colon
        # at either end. Returns the column count, or 0 if this is not a delimiter row.
        function delim_cells(line,   n, i) {
            n = table_cells(line)
            if (n == 0) { return 0 }
            for (i = 2; i <= n + 1; i++) {
                if (tcell[i] !~ /^ *:?-+:? *$/) { return 0 }
            }
            return n
        }

        # A header row: a table row that is not itself a delimiter and that renders as something.
        # Its text is prose and is not judged; only that it is there and how many columns it has.
        function header_cells(line,   n, i, seen) {
            if (delim_cells(line) > 0) { return 0 }
            n = table_cells(line)
            if (n == 0) { return 0 }
            seen = 0
            for (i = 2; i <= n + 1; i++) {
                if (!is_blank(tcell[i])) { seen = 1 }
            }
            return seen ? n : 0
        }

        # Empty means "renders as nothing": nothing survives once HTML tags, the Unicode blanks a
        # renderer collapses, and the ASCII blanks and control bytes are taken out. A cell holding
        # only <span></span> or a lone <br/> is therefore empty, because that is what a reader sees.
        function is_blank(cell,   c) {
            c = cell
            gsub(/<[^<>]*>/, "", c)
            gsub(ENT_BLANK, " ", c)
            gsub(UNI_BLANK, " ", c)
            gsub(ASCII_BLANK, "", c)
            return c == ""
        }

        # Remembers the last two lines that a renderer would see as block syntax, so the first
        # crate row can be judged against the two lines directly above it. A line inside a fenced
        # block is remembered as empty: whatever it spells, it is not table syntax.
        function shift(seen) { prev2 = prev1; prev1 = seen }

        {
            line = $0
            if (substr(line, length(line), 1) == CR) {
                line = substr(line, 1, length(line) - 1)
            }

            # Inside a fenced block nothing else is syntax -- not a heading, not a comment, not a
            # row -- until the fence closes.
            if (in_fence) {
                if (is_fence_close(line)) { in_fence = 0 }
                shift("")
                next
            }

            line = strip_comments(line)

            if (match(line, /^ *(```+|~~~+)/)) {
                marker = substr(line, RSTART, RLENGTH)
                sub(/^ +/, "", marker)
                fence_char = substr(marker, 1, 1)
                fence_len = length(marker)
                in_fence = 1
                shift("")
                next
            }

            # A trailing space on an ATX heading is legal and renders identically, so it must not
            # unmatch the section and report an intact table as rowless.
            if (line ~ /^## Crates[ \t]*$/) { in_section = 1; shift(""); next }
            if (line ~ /^## /)        { in_section = 0; shift(""); next }
            if (!in_section)          { shift(""); next }

            if (line !~ /^\| *`[A-Za-z0-9_-]+` *\|/) { shift(line); next }

            # The first crate row is where the context of the table is judged, on exactly the
            # two lines DIRECTLY above it: a delimiter, and above that a header agreeing with it on
            # column count. Anything else -- a blank line, a decoy table further up, a header of a
            # different width -- renders as no table, however intact the rows below it look.
            if (nrows == 0) {
                dcols = delim_cells(prev1)
                hcols = header_cells(prev2)
                if (dcols == 0) {
                    print "NODELIM\t"
                } else if (hcols == 0) {
                    print "NOHEADER\t"
                } else if (hcols != dcols) {
                    print "MISMATCH\t" hcols " header column(s) over a " dcols "-column delimiter"
                } else {
                    print "DELIM\tok"
                }
            }

            nrows++
            row = line
            gsub(/\\\|/, SENTINEL, row)
            n = split(row, cell, "|")
            name = cell[2]
            gsub(/[` ]/, "", name)
            print "NAME\t" name

            if (n != 5) {
                print "BAD\t" name " has " (n - 2) " cell(s); a crate row is Crate | Purpose | Status"
                shift(line)
                next
            }
            purpose = cell[3]
            status = cell[4]
            gsub(SENTINEL, "|", purpose)
            gsub(SENTINEL, "|", status)
            if (is_blank(purpose)) { print "BAD\t" name " has an empty Purpose cell" }
            if (is_blank(status))  { print "BAD\t" name " has an empty Status cell" }
            shift(line)
        }
    ' "$readme"
)"

rows="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "NAME" { print $2 }')"
malformed="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "BAD" { print $2 }')"
delimiter="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "DELIM" { print $2 }')"
no_delim="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "NODELIM" { print "1" }')"
no_header="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "NOHEADER" { print "1" }')"
mismatch="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "MISMATCH" { print $2 }')"

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

if [ -n "$no_delim" ]; then
    fail=1
    echo "check-readme-crates: in $readme, the line directly above the first crate row is not a"
    echo "  '| --- | --- | --- |' delimiter row. A delimiter somewhere further up does not make a"
    echo "  table: a blank line after it, an unrelated table earlier in the section, or four"
    echo "  spaces of indentation each leave the rows rendering as a paragraph of literal pipes,"
    echo "  so every row below documents nothing."
elif [ -n "$no_header" ]; then
    fail=1
    echo "check-readme-crates: in $readme, the delimiter above the first crate row has no header"
    echo "  row directly above it. A delimiter with nothing above it starts no table, so the rows"
    echo "  below render as a paragraph of literal pipes. What the headings are CALLED is not"
    echo "  checked -- only that a row is there."
elif [ -n "$mismatch" ]; then
    fail=1
    echo "check-readme-crates: in $readme, the crates table header and its delimiter disagree on"
    echo "  how many columns the table has:"
    echo "$mismatch" | sed 's/^/  /'
    echo "  Markdown renders no table at all when they disagree, whatever the rows below say."
elif [ -z "$delimiter" ]; then
    fail=1
    echo "check-readme-crates: internal error: the crates table context was never judged"
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
    echo "  a crate row is recognised ONLY as: | \`crate-name\` | Purpose | Status |"
    echo "  -- the pipe in column 1, the crate name alone inside a code span. That pattern is"
    echo "  deliberately narrow, so legal Markdown it does not recognise (a missing leading or"
    echo "  trailing pipe, an indented row, a linked or annotated crate cell, prose after the code"
    echo "  span) is reported here as a missing crate. Check the row's shape before adding one."
    echo "  Its authority is the crate's own source -- the"
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
