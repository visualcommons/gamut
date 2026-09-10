#!/usr/bin/env bash
# The README's "## Crates" table is hand-maintained prose, and nothing read it. By issue #425 it
# had drifted far enough that twenty-three of its twenty-eight rows needed correcting -- five
# shipped crates were still described as unstarted scaffolding citing a closed issue -- and four
# crates (gamut-codec-abi, gamut-dng, gamut-jpeg, gamut-tonemap) had no row at all.
#
# This guard checks what is mechanical about the table -- its *membership*, the *shape* of a row,
# that the table is a table, and the handful of claim forms `cargo metadata` can decide:
#
#   * every workspace crate has a row, so a new crate cannot be added without documenting it;
#   * every row names a crate that still exists, so a renamed or deleted crate cannot be left
#     behind as a phantom row. A row is recognised by a backticked cargo package name, so the
#     character set is cargo's own -- letters, digits, `-` and `_`, in either case -- and a
#     phantom row cannot hide behind a capital or an underscore;
#   * no crate is listed twice, so the table stays a bijection rather than a set;
#   * every `gamut`-prefixed name a Purpose or Status cell backticks is a workspace crate too, so
#     a rename leaves no phantom behind in the prose either;
#   * every crate row is a three-cell row whose Purpose and Status each *render as something*:
#     a cell holding one non-breaking space, one tab, or one empty HTML element is empty, because
#     that is what a reader sees;
#   * a row inside an HTML comment or a fenced code block is not a row. Both render as something
#     other than a table cell, so a crate documented only there is documented nowhere, and the
#     membership check must see it as missing rather than as present;
#   * EVERY crate row in the section lives inside ONE rendered table. A table is a header row
#     immediately followed by a GFM delimiter row of the same width, neither indented four spaces
#     or more, and it ends at the first line that is not a table row. That is a claim about the
#     whole section rather than about the first row's two neighbours: a blank line between two
#     crate rows, a decoy table earlier in the section donating its head, a deleted delimiter, a
#     blanked header and a header of the wrong width all leave some or all of the rows rendering
#     as a paragraph of literal pipes while each row's bytes stay intact, and all of them fail
#     here.
#
# A set of claim FORMS is checked against `cargo metadata`, and they are the only prose this
# guard reads. Each is opt-in and marker-driven: text that writes no marker claims nothing and is
# not checked. Write a claim a reader could check against cargo in one of these forms, or do not
# write it -- a hand-maintained list of crates is exactly the defect this file exists to catch,
# one level down.
#
# The forms are deliberately NOT enumerated here. An enumeration in the documentation of an
# enumeration checker is the one place a hand-written list must not be, and this header was stale
# the moment the fifth check landed: the same commit left it saying "four". `claims()` below is the
# list. Every form is one `match()` in it, tagged and documented at that match site, and every
# failure message names the form it rejects and the shape it wants:
#
#     grep -n 'CLAIM FORM:' tooling/check-readme-crates/run.sh
#
# In every name-list form the list ends at the first character that is not a backticked name, a
# comma, a colon, a space or the word "and", so ordinary prose may follow it on the same line.
#
# WHAT THE CONTRACT COVERS. The whole `## Crates` section, not only its rows. The structural
# checks always judged the section; the claim checks do too, so a machine-decidable claim written
# as ordinary prose above or below the table -- or in a `###` sub-heading inside it -- is read
# exactly as it would be inside a cell. Two forms -- the version token and the `consumed
# by`/`always-on` lists -- name no subject of their own and take the row's crate as their subject,
# so outside a crate row they have nobody to be about: writing one there is a failure rather than
# a silent pass, and the message says to move it into a row. The one exception is a fenced or
# indented code block, which is outside every check here: its content is a code sample, not a
# claim this workspace has to answer for.
#
# WHAT THE TABLE IS FORBIDDEN TO WRITE, and why forbidding beats widening. A check that reads
# backticked names can only be as good as the assumption that names are backticked, and that
# assumption was not enforced: an unbackticked `gamut-ifdd` in a cell sailed past the cite check.
# The fix is not a wider pattern -- widening is how a phantom gets in -- but the requirement made
# real. Inside the section:
#
#   * a `gamut-`/`gamut_` compound OUTSIDE a code span is a failure, whether or not it names a
#     real crate, because that is the precondition every name check rests on. The bare word
#     `gamut` is exempt: it is the project's name in English as well as the umbrella's package
#     name, and requiring a code span around every mention of it would reject the prose this
#     README is made of. A fenced or indented code block is exempt too -- there a compound is a
#     sample of Rust or of a shell line, where `gamut_png` is the correct spelling;
#   * the underscore spelling of a workspace crate is REJECTED, deliberately, and this is the one
#     place that decision is written down. `gamut_ifd` is a Rust identifier, not a cargo package
#     name; cargo publishes `gamut-ifd` and `cargo add gamut_ifd` does not resolve. Accepting it
#     would also split the guard against itself, because the crate-cell pattern reads `_` on
#     purpose so a phantom row cannot hide behind one -- one half would fold the spelling while
#     the other treats it as a distinct name. Cells name packages; the umbrella's module aliases
#     (`gamut::png`) are how the Rust side is spelled in prose.
#
# CRLF input is accepted: a trailing carriage return is removed from every line before anything is
# matched, so a CRLF README does not silently lose its heading and report itself as rowless.
#
# The `## Crates` heading is matched in every form CommonMark gives it: any ATX level-2 heading
# with the text `Crates`, indented up to three spaces, with or without a closing `##` sequence, or
# the setext form underlined with dashes. Any level-1 or level-2 heading ends the section; a
# `###` or deeper heading does not, so a sub-table inside the section is still part of it.
#
# What is deliberately NOT checked, and why. These are limitations, not oversights, and this
# comment is the one place they are written down:
#   * The *wording* of the Purpose and Status cells, beyond the claim forms. They are
#     prose a human maintains, and their authority is **the crate's own source** -- the code that
#     ships. No single file outranks it: `STATUS.md` can list a shipped module as deferred (issue
#     #545) and a module doc can defer a capability the crate's own `EncodeImage` impls already
#     provide (issue #560) -- which is why the `gamut-avif` row states 8/10/12-bit encode against
#     a doc comment that still calls 10/12-bit deferred. `lib.rs`, `Cargo.toml` and `STATUS.md`
#     are where to look first, in that order, but each is a summary and a row must be true of the
#     crate, not faithful to a file. A text gate over the cells would fossilise a particular
#     phrasing, and a generated table would move prose a human writes into a generator.
#   * The header row's own text. Its *presence* directly above the delimiter and its *column
#     count* are asserted; what the three column headings are called is prose like any other cell.
#   * Anything that needs the network. Whether crates.io serves the version a manifest declares
#     cannot be decided here, so the table states the manifest version and [Releases](#releases)
#     states the gaps.
#   * A row whose crate cell is written in legal Markdown this pattern does not recognise -- an
#     omitted leading OR trailing pipe, a linked or annotated crate cell, prose after the code
#     span. Each is reported as a *missing crate*, and the failure message says so, because
#     widening the pattern is how a phantom row gets in. Narrow and loud beats wide and quiet.
#   * Whether an HTML element in a cell renders. Every tag is treated as rendering nothing, so a
#     cell whose only content is `<img ...>` is reported empty. Telling a replaced element from a
#     wrapper needs an HTML model, and a crates-table cell that says nothing in plain text
#     documents nothing to a reader of the file.
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

# One pass over the file emits every stream -- `NAME` for membership, `BAD` for shape, `ORPHAN`
# and `SPLIT` for the table context, `VER`/`CONS`/`ALWAYS`/`FEAT`/`CITE` for the claims -- so the
# checks can never disagree about which lines are crate rows. Bounded to the "## Crates" section
# so the README's other tables (the `mise run ...` table) can never be mistaken for a crate row.
#
# LC_ALL=C makes every regexp below byte-wise, which is what the octal escapes assume; the awk
# is POSIX (no gensub, no interval expressions, dynamic regexps built as strings) so it behaves
# the same under mawk, which is what `awk` is on the CI runner.
#
# `CHECK_README_AWK` picks the interpreter, unquoted so `CHECK_README_AWK='gawk --posix'` works.
# The portability claim is only worth what someone can re-run: `fixtures.sh` beside this file
# drives the whole battery through here once per interpreter available on the machine.
scan="$(
    LC_ALL=C ${CHECK_README_AWK:-awk} '
        BEGIN {
            # An escaped pipe is content, not a column separator, so it is swapped for a control
            # byte before the split and swapped back before the cell is judged.
            SENTINEL = "\001"
            CR = sprintf("%c", 13)
            # ASCII space and every C0/DEL control byte.
            ASCII_BLANK = "[ \001-\037\177]"
            # The UTF-8 encodings of the Unicode blanks a Markdown renderer shows as nothing:
            # U+00A0, U+1680, U+2000-U+200F, U+2028, U+2029, U+202F, U+205F, U+2060, U+3000 and
            # U+FEFF. Without this fold a single non-breaking space reconstructs exactly the
            # degenerate row the shape check exists to reject.
            UNI_BLANK = "\302\240|\341\232\200|\342\200[\200-\217\250\251\257]"
            UNI_BLANK = UNI_BLANK "|\342\201[\237\240]|\343\200\200|\357\273\277"
            # The same blanks written as HTML character references, which a Markdown renderer
            # resolves before it shows the cell. The named list is the common set, NOT an
            # exhaustive one -- HTML5 names some 2000 references -- but the numeric lists below
            # cover every code point the two lists above name, in decimal and in hex, with
            # leading zeros allowed. A named reference whose numeric twin was missing was the
            # whole hole: `&lrm;` folded while `&#8206;` did not.
            ENT_BLANK = "&(nbsp|NonBreakingSpace|ensp|emsp|emsp13|emsp14|numsp|puncsp|thinsp"
            ENT_BLANK = ENT_BLANK "|hairsp|VeryThinSpace|MediumSpace|ThickSpace|ThinSpace"
            ENT_BLANK = ENT_BLANK "|zwnj|zwj|ZeroWidthSpace|NegativeThinSpace|NoBreak|lrm|rlm);"
            ENT_BLANK = ENT_BLANK "|&#0*(160|5760|819[2-9]|820[0-7]|8232|8233|8239|8287|8288"
            ENT_BLANK = ENT_BLANK "|12288|65279);"
            ENT_BLANK = ENT_BLANK "|&#[xX]0*([Aa]0|1680|200[0-9A-Fa-f]|202[89Ff]|205[Ff]"
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

        function trim(s) {
            sub(/^[ \t]+/, "", s)
            sub(/[ \t]+$/, "", s)
            return s
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

        # The level of an ATX heading (1-6), with its text left in `htext`, or 0. Up to three
        # spaces of indentation is legal; four is an indented code block. A closing run of `#`
        # separated from the text by whitespace is a closing sequence and is not text, so
        # `## Crates ##` is the heading `Crates`, while `## Crates#` is not.
        function atx_level(line,   m, lvl, t) {
            m = line
            if (m ~ /^    /) { return 0 }
            sub(/^ */, "", m)
            lvl = 0
            while (substr(m, lvl + 1, 1) == "#") { lvl++ }
            if (lvl < 1 || lvl > 6) { return 0 }
            t = substr(m, lvl + 1)
            if (t != "" && t !~ /^[ \t]/) { return 0 }
            t = trim(t)
            if (t ~ /(^|[ \t])#+$/) {
                sub(/[ \t]*#+$/, "", t)
                t = trim(t)
            }
            htext = t
            return lvl
        }

        # 1 for a setext `===` underline, 2 for a `---` one, 0 otherwise. Whether it underlines
        # anything is the caller`s business: only a paragraph line can be underlined.
        function setext_rule(line,   m) {
            m = line
            if (m ~ /^    /) { return 0 }
            sub(/^ */, "", m)
            sub(/[ \t]*$/, "", m)
            if (m ~ /^=+$/) { return 1 }
            if (m ~ /^-+$/) { return 2 }
            return 0
        }

        # Splits a pipe-delimited line into `tcell[2..]` and returns the cell count, or 0 if the
        # line is not a table row at all. Up to three spaces of indentation is legal; four or more
        # is an indented code block, not a table -- so an indented delimiter is no delimiter. An
        # escaped pipe is content, so it is swapped out before the split here too, exactly as the
        # row shape check does.
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

        # A paragraph line: something a setext rule below it would turn into a heading. Not blank,
        # not a table row, not itself an ATX heading.
        function is_para(line) {
            if (trim(line) == "") { return 0 }
            if (table_cells(line) > 0) { return 0 }
            if (atx_level(line) > 0) { return 0 }
            return 1
        }

        # Collects the backticked crate names that follow a claim marker, stopping at the first
        # character that is neither a name, a separator, nor the word "and" -- so prose may follow
        # the list on the same line.
        function name_list(rest,   out, tok) {
            out = ""
            while (1) {
                sub(/^[ ,:]+/, "", rest)
                if (rest ~ /^and[ ]/) { rest = substr(rest, 4); continue }
                if (rest !~ /^`[A-Za-z0-9_-]+`/) { return out }
                match(rest, /^`[A-Za-z0-9_-]+`/)
                tok = substr(rest, 2, RLENGTH - 2)
                out = (out == "") ? tok : out " " tok
                rest = substr(rest, RLENGTH + 1)
            }
        }

        # Every backticked lower-case identifier in the text, which is the set of crates a feature
        # or an external dependency named here may belong to. `name` is the row`s own crate, or ""
        # for prose outside a row, which owns nothing but what it names. The shell keeps only the
        # workspace members.
        function cited(name, text,   out, rest) {
            out = name
            rest = text
            while (match(rest, /`[A-Za-z0-9_-]+`/)) {
                out = (out == "") ? substr(rest, RSTART + 1, RLENGTH - 2) \
                                  : out " " substr(rest, RSTART + 1, RLENGTH - 2)
                rest = substr(rest, RSTART + RLENGTH)
            }
            return out
        }

        # CLAIM FORM: a backticked `gamut`-prefixed name asserts that such a crate exists, and
        # cargo settles that: a renamed or deleted crate leaves a phantom behind in a Purpose cell
        # exactly as it does in a crate cell, and only the crate cell was ever read. The underscore
        # spelling is a name like any other here and so fails, which the header explains.
        function cites(subject, text,   rest, tok, out) {
            out = ""
            rest = text
            while (match(rest, /`gamut[A-Za-z0-9_-]*`/)) {
                tok = substr(rest, RSTART + 1, RLENGTH - 2)
                out = (out == "") ? tok : out " " tok
                rest = substr(rest, RSTART + RLENGTH)
            }
            if (out != "") { print "CITE\t" subject "\t" out }
        }

        # A `gamut-`/`gamut_` compound written outside a code span. Every name check downstream
        # reads code spans only, so an unbackticked name is not a name the guard can see; the
        # requirement is enforced here rather than assumed. The bare word `gamut` is exempt --
        # it is English in this README as well as a package name.
        function unbackticked(subject, text,   rest, tok, out) {
            rest = text
            gsub(/`[^`]*`/, " ", rest)
            out = ""
            while (match(rest, /gamut[-_][A-Za-z0-9_-]*/)) {
                tok = substr(rest, RSTART, RLENGTH)
                out = (out == "") ? tok : out " " tok
                rest = substr(rest, RSTART + RLENGTH)
            }
            if (out != "") { print "UNBT\t" subject "\t" out }
        }

        # Emits the machine-checkable claims a text makes. Each is opt-in: text that writes no
        # marker claims nothing. `name` is the row`s crate, or "" when this is prose in the
        # section rather than a crate row -- the two subject-bearing forms are then refused
        # instead of being read, because they would have nobody to be about.
        function claims(subject, name, text,   s, tok, nxt, pre, qual, lst, at, len) {
            cites(subject, text)
            # CLAIM FORM: a `vN`/`vN.M` token -- the FIRST one in the row -- must agree with that
            # crate`s own `Cargo.toml` version, to the precision written (`v2` checks the major,
            # `v0.2` the major and minor). This is what makes a version the most checkable thing
            # a cell can carry.
            s = " " text
            while (match(s, /[^A-Za-z0-9_.]v[0-9]+(\.[0-9]+)*/)) {
                tok = substr(s, RSTART + 2, RLENGTH - 2)
                nxt = substr(s, RSTART + RLENGTH, 1)
                s = substr(s, RSTART + RLENGTH)
                if (nxt !~ /[A-Za-z0-9_]/) {
                    if (name == "") { print "SUBJ\t" subject "\ta version token (v" tok ")" }
                    else { print "VER\t" name "\t" tok }
                    break
                }
            }
            # CLAIM FORM: `consumed by` followed by backticked crate names -- that set must be
            # exactly the workspace crates declaring a normal or build dependency on the row`s
            # crate. Dev-dependencies are excluded, as they are for `check-release-deps`: they are
            # not what a consumer links.
            if (match(text, /consumed by/)) {
                if (name == "") { print "SUBJ\t" subject "\ta `consumed by` list" }
                else { print "CONS\t" name "\t" name_list(substr(text, RSTART + RLENGTH)) }
            }
            # CLAIM FORM: `always-on dependency`/`always-on dependencies` followed by backticked
            # crate names -- that set must be exactly the workspace crates the row`s crate depends
            # on non-optionally. Naming none asserts that there are none.
            if (match(text, /always-on dependenc(y|ies)/)) {
                if (name == "") { print "SUBJ\t" subject "\tan `always-on dependenc...` list" }
                else { print "ALWAYS\t" name "\t" name_list(substr(text, RSTART + RLENGTH)) }
            }
            # CLAIM FORM: `Cargo feature`/`Cargo features` immediately followed by backticked
            # names -- each must be declared by one of the workspace crates the text names,
            # because a row may legitimately point at the umbrella`s feature for the seam it
            # describes. Writing `default Cargo feature` instead additionally requires that
            # crate`s `default` list to enable it. Every occurrence is read, not only the first.
            # The marker carries `Cargo` because bare `feature`/`features` is an ordinary English
            # verb: "the crate features `chunk` walking" was read as a feature claim and rejected,
            # and a guard that rejects legal prose is a guard someone turns off.
            s = text
            while (match(s, /Cargo features?[ ]+`/)) {
                # name_list() and cited() both call match(), so the offsets of the marker are
                # saved before either runs -- reading RSTART back afterwards would not advance.
                at = RSTART
                len = RLENGTH
                pre = substr(s, 1, at - 1)
                qual = (pre ~ /default[ ]*$/) ? "default" : "any"
                lst = name_list(substr(s, at + len - 1))
                if (lst != "") {
                    print "FEAT\t" subject "\t" qual "\t" lst "\t" cited(name, text)
                }
                s = substr(s, at + len)
            }
            # CLAIM FORM: `external dependency`/`external dependencies` immediately followed by
            # backticked names -- each must be a non-dev dependency of one of the workspace crates
            # the text names. This is the marker that settles a name cargo knows but the cite
            # check cannot read, because no rule separates an external crate name from a module or
            # a type name in a code span: under a marker, the writer has said which it is.
            s = text
            while (match(s, /external dependenc(y|ies)[ ]+`/)) {
                at = RSTART
                len = RLENGTH
                lst = name_list(substr(s, at + len - 1))
                if (lst != "") {
                    print "EXTDEP\t" subject "\t" lst "\t" cited(name, text)
                }
                s = substr(s, at + len)
            }
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
                in_table = 0
                prev = ""
                prev_para = 0
                next
            }

            cur = strip_comments(line)

            if (match(cur, /^ *(```+|~~~+)/)) {
                marker = substr(cur, RSTART, RLENGTH)
                sub(/^ +/, "", marker)
                fence_char = substr(marker, 1, 1)
                fence_len = length(marker)
                in_fence = 1
                in_table = 0
                prev = ""
                prev_para = 0
                next
            }

            lvl = atx_level(cur)
            if (lvl > 0) {
                htitle = htext
            } else {
                srule = setext_rule(cur)
                if (srule > 0 && prev_para) { lvl = srule; htitle = trim(prev) }
            }
            if (lvl > 0) {
                in_table = 0
                if (lvl <= 2) { in_section = (htitle == "Crates") ? 1 : 0 }
                # A `###` sub-heading does not end the section, so its text is section text and
                # is read like any other line -- names and claims alike. The `## Crates` heading
                # itself is read too, which costs nothing and keeps "every line of the section"
                # true without an exception.
                if (in_section) { unbackticked("line " NR, cur); claims("line " NR, "", cur) }
                prev = ""
                prev_para = 0
                next
            }

            if (!in_section) {
                prev = cur
                prev_para = is_para(cur)
                next
            }

            # The contract is the whole section, not only its rows. A fenced or indented code
            # block is exempt from all of it, names and claims alike: there a `gamut_png` is a
            # sample of Rust or of a shell line, where the underscore spelling is the correct one,
            # and a marker is a word in a code sample rather than a claim about this workspace.
            if (cur !~ /^    /) { unbackticked("line " NR, cur) }

            # A table runs from its delimiter row until the first line that is not a table row.
            # Tracking it for the whole section is what makes "every crate row is in ONE table"
            # answerable, rather than only "the first crate row has a head above it".
            ncells = table_cells(cur)
            if (in_table && ncells == 0) { in_table = 0 }
            if (!in_table) {
                d = delim_cells(cur)
                if (d > 0) {
                    h = header_cells(prev)
                    if (h == d) {
                        in_table = 1
                        ntable++
                        # A crate row directly above this delimiter IS this table`s header row,
                        # so it renders inside the table rather than outside it.
                        if (ncrate > 0 && crline[ncrate] == NR - 1 && crtable[ncrate] == 0) {
                            crtable[ncrate] = ntable
                        }
                    } else {
                        nhint++
                        if (h == 0) {
                            hint[nhint] = "line " NR ": a delimiter row with no header row directly above it"
                        } else {
                            hint[nhint] = "line " NR ": a " h "-column header over a " d "-column delimiter"
                        }
                    }
                }
            }

            row = cur
            if (row ~ /^    /) { prev = cur; prev_para = is_para(cur); next }
            sub(/^ */, "", row)
            sub(/[ \t]*$/, "", row)
            gsub(/\\\|/, SENTINEL, row)
            if (row !~ /^\| *`[A-Za-z0-9_-]+` *\|/ || substr(row, length(row), 1) != "|") {
                # Not a crate row, so it is prose in the section: the claim forms that name their
                # own subject are checked here, and the two that borrow the row`s crate as their
                # subject are refused rather than ignored.
                claims("line " NR, "", cur)
                prev = cur
                prev_para = is_para(cur)
                next
            }

            ncrate++
            crline[ncrate] = NR
            crtable[ncrate] = in_table ? ntable : 0
            n = split(row, cell, "|")
            name = cell[2]
            gsub(/[` ]/, "", name)
            crname[ncrate] = name

            if (n != 5) {
                nbad++
                bad[nbad] = name " has " (n - 2) " cell(s); a crate row is Crate | Purpose | Status"
            } else {
                purpose = cell[3]
                status = cell[4]
                gsub(SENTINEL, "|", purpose)
                gsub(SENTINEL, "|", status)
                if (is_blank(purpose)) { nbad++; bad[nbad] = name " has an empty Purpose cell" }
                if (is_blank(status))  { nbad++; bad[nbad] = name " has an empty Status cell" }
                claims("the `" name "` row", name, purpose " " status)
            }

            prev = cur
            prev_para = is_para(cur)
        }

        END {
            for (i = 1; i <= ncrate; i++) { print "NAME\t" crname[i] }
            for (i = 1; i <= nbad; i++)   { print "BAD\t" bad[i] }

            orphan = ""
            ndistinct = 0
            for (i = 1; i <= ncrate; i++) {
                if (crtable[i] == 0) {
                    orphan = (orphan == "") ? crname[i] : orphan " " crname[i]
                } else if (!(crtable[i] in tseen)) {
                    tseen[crtable[i]] = 1
                    ndistinct++
                }
            }
            if (ncrate > 0) {
                if (orphan != "") {
                    print "ORPHAN\t" orphan
                } else if (ndistinct > 1) {
                    print "SPLIT\t" ndistinct
                }
            }
            for (i = 1; i <= nhint; i++) { print "HINT\t" hint[i] }
        }
    ' "$readme"
)"

rows="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "NAME" { print $2 }')"
malformed="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "BAD" { print $2 }')"
orphans="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "ORPHAN" { print $2 }')"
split_tables="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "SPLIT" { print $2 }')"
hints="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "HINT" { print $2 }')"

test -n "$rows" || {
    echo "check-readme-crates: found no crate rows under '## Crates' in $readme"
    echo "  either the '## Crates' heading is missing or is not a level-2 heading with that exact"
    echo "  text, or every row under it is hidden: a row inside an HTML comment or a fenced code"
    echo "  block does not count."
    exit 1
}

fail=0

if [ -n "$malformed" ]; then
    fail=1
    echo "check-readme-crates: malformed crate rows in the $readme crates table:"
    echo "$malformed" | sed 's/^/  /'
fi

if [ -n "$orphans" ]; then
    fail=1
    echo "check-readme-crates: in $readme, these crate rows do not render inside a table:"
    echo "$orphans" | tr ' ' '\n' | sed 's/^/  /'
    echo "  A table is a header row IMMEDIATELY followed by a '| --- | --- | --- |' delimiter of"
    echo "  the same width, neither indented four spaces or more, and it ends at the first line"
    echo "  that is not a table row. A blank line between two rows, a deleted delimiter, a blanked"
    echo "  header or a header of the wrong width each leave the rows below rendering as a"
    echo "  paragraph of literal pipes, however intact their bytes look."
    if [ -n "$hints" ]; then
        echo "  Near misses seen in the section:"
        echo "$hints" | sed 's/^/    /'
    fi
elif [ -n "$split_tables" ]; then
    fail=1
    echo "check-readme-crates: in $readme, the crate rows are spread over $split_tables separate"
    echo "  tables. They document one list of crates and must render as one table; a second table"
    echo "  in the section (a decoy above the real one, or a blank line splitting it) is how rows"
    echo "  keep passing every other check while rendering somewhere a reader will not read them."
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

# The same source of truth `mise run versions` reads: --no-deps lists workspace members only. It
# is read once and every derived claim below is answered from this one document, so two checks
# can never disagree about the graph. `check-release-deps` runs the same command in the same CI
# job, so nothing here needs a build.
metadata="$(cargo metadata --no-deps --format-version 1)" || {
    echo "check-readme-crates: could not read the workspace crate list from cargo metadata"
    exit 1
}

workspace_crates="$(printf '%s' "$metadata" | jq -r '.packages | sort_by(.name)[] | .name')" || {
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
    echo "  -- a pipe first on the line (up to three spaces of indent), the crate name alone"
    echo "  inside a code span, and a closing pipe at the end. That pattern is deliberately"
    echo "  narrow, so legal Markdown it does not recognise (a missing leading or trailing pipe,"
    echo "  a linked or annotated crate cell, prose after the code span) is reported here as a"
    echo "  missing crate. Check the row's shape before adding one."
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

# Normalises a claimed name list so a set comparison does not depend on the order or the spacing
# a sentence happens to use.
normalise_set() {
    printf '%s\n' "$1" | tr ' ' '\n' | sed '/^$/d' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ $//'
}

# `vN` / `vN.M`: the row's first version token against the crate's own manifest version, compared
# to the precision the row wrote.
version_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "VER" { print $2 "\t" $3 }')"
if [ -n "$version_claims" ]; then
    manifest_versions="$(printf '%s' "$metadata" | jq -r '.packages[] | "\(.name)\t\(.version)"')"
    version_errors="$(
        printf '%s\n' "$version_claims" | while IFS="$(printf '\t')" read -r crate claimed; do
            actual="$(printf '%s\n' "$manifest_versions" | awk -F'\t' -v c="$crate" '$1 == c { print $2 }')"
            if [ -n "$actual" ]; then
                prefix="$(printf '%s\n' "$actual" | awk -F. -v t="$claimed" '
                    BEGIN { n = split(t, want, ".") }
                    { s = $1; for (i = 2; i <= n; i++) { s = s "." $i } print s }
                ')"
                if [ "$prefix" != "$claimed" ]; then
                    echo "  $crate: the row cites v$claimed, its Cargo.toml declares $actual"
                fi
            fi
        done
        true
    )"
    if [ -n "$version_errors" ]; then
        fail=1
        echo "check-readme-crates: version claims in $readme that the workspace manifests refute:"
        echo "$version_errors"
        echo "  A row's version is its own Cargo.toml's, to whatever precision the row writes."
    fi
fi

# `consumed by`: the claimed consumer set against the workspace crates that declare a normal or
# build dependency on the row's crate.
consumer_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "CONS" { print $2 "\t" $3 }')"
if [ -n "$consumer_claims" ]; then
    actual_consumers="$(printf '%s' "$metadata" | jq -r '
        [ .packages[] | . as $p | $p.dependencies[]
          | select((.kind // "normal") != "dev")
          | { consumer: $p.name, dependency: .name } ]
        | group_by(.dependency)[]
        | "\(.[0].dependency)\t\(map(.consumer) | unique | sort | join(" "))"
    ')"
    consumer_errors="$(
        printf '%s\n' "$consumer_claims" | while IFS="$(printf '\t')" read -r crate claimed; do
            actual="$(printf '%s\n' "$actual_consumers" | awk -F'\t' -v c="$crate" '$1 == c { print $2 }')"
            want="$(normalise_set "$actual")"
            got="$(normalise_set "$claimed")"
            if [ "$want" != "$got" ]; then
                echo "  $crate: the row says [$got]; cargo metadata says [$want]"
            fi
        done
        true
    )"
    if [ -n "$consumer_errors" ]; then
        fail=1
        echo "check-readme-crates: 'consumed by' lists in $readme that cargo metadata refutes:"
        echo "$consumer_errors"
        echo "  Dev-dependencies are excluded, as they are for check-release-deps."
    fi
fi

# `always-on dependency`/`dependencies`: the claimed set against the row crate's non-optional
# workspace dependencies. Naming none asserts that there are none.
always_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "ALWAYS" { print $2 "\t" $3 }')"
if [ -n "$always_claims" ]; then
    actual_always="$(printf '%s' "$metadata" | jq -r '
        [ .packages[].name ] as $members
        | .packages[]
        | "\(.name)\t\([ .dependencies[]
              | select((.kind // "normal") != "dev")
              | select(.optional | not)
              | .name
              | select(. as $n | $members | index($n)) ] | unique | sort | join(" "))"
    ')"
    always_errors="$(
        printf '%s\n' "$always_claims" | while IFS="$(printf '\t')" read -r crate claimed; do
            actual="$(printf '%s\n' "$actual_always" | awk -F'\t' -v c="$crate" '$1 == c { print $2 }')"
            want="$(normalise_set "$actual")"
            got="$(normalise_set "$claimed")"
            if [ "$want" != "$got" ]; then
                echo "  $crate: the row says [$got]; cargo metadata says [$want]"
            fi
        done
        true
    )"
    if [ -n "$always_errors" ]; then
        fail=1
        echo "check-readme-crates: 'always-on dependency' lists in $readme that cargo metadata refutes:"
        echo "$always_errors"
        echo "  An optional dependency is not always on, and only workspace crates are counted."
    fi
fi

# Every `gamut`-prefixed name a cell backticks must be a workspace crate. The phantom check above
# reads the crate cell only, so before this a Purpose cell could name a crate that no longer
# exists -- the same defect the crate cell has been guarded against since the first commit.
cite_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "CITE" { print $2 "\t" $3 }')"
if [ -n "$cite_claims" ]; then
    cite_errors="$(
        printf '%s\n' "$cite_claims" | while IFS="$(printf '\t')" read -r subject names; do
            for cited in $names; do
                if ! printf '%s\n' "$workspace_crates" | grep -qxF -- "$cited"; then
                    echo "  $subject names \`$cited\`, which is not a workspace crate"
                fi
            done
        done
        true
    )"
    if [ -n "$cite_errors" ]; then
        fail=1
        echo "check-readme-crates: $readme names crates that do not exist:"
        echo "$cite_errors"
        echo "  drop the mention, or fix the crate name it misspells. The underscore spelling of a"
        echo "  real crate is rejected here on purpose: a cell names a cargo package, and cargo"
        echo "  publishes the hyphenated name (see the header of this script for the whole reason)."
    fi
fi

# The precondition every name check above rests on: names are inside code spans. A `gamut-`/
# `gamut_` compound written as bare text is a failure whether or not it names a real crate,
# because an unbackticked name is invisible to the cite check -- an unbackticked `gamut-ifdd`
# passed every check this script had.
unbackticked="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "UNBT" { print "  " $2 ": " $3 }')"
if [ -n "$unbackticked" ]; then
    fail=1
    echo "check-readme-crates: in $readme, crate names written without a code span:"
    echo "$unbackticked"
    echo "  Inside '## Crates', write every \`gamut-\`/\`gamut_\` compound in a code span, so the"
    echo "  checks that read crate names can see it. The bare word 'gamut' is exempt (it is"
    echo "  English here as well as a package name), and so is a fenced or indented code block."
fi

# A version token, a `consumed by` list or an `always-on` list takes the row's crate as its
# subject. Outside a crate row there is no such crate, so the claim cannot be decided; it is
# refused rather than silently skipped, which is what made prose in this section invisible.
subjectless="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "SUBJ" { print "  " $2 ": " $3 }')"
if [ -n "$subjectless" ]; then
    fail=1
    echo "check-readme-crates: in $readme, claims outside a crate row that name no crate to be about:"
    echo "$subjectless"
    echo "  These forms are decided against the crate whose row they sit in. Move the claim into"
    echo "  that crate's row, or write the sentence without the form's marker."
fi

# `feature`/`features`: each named feature must be declared by one of the workspace crates the row
# talks about -- its own crate, or another one it names -- because a row may legitimately cite the
# umbrella's feature for the seam the row describes. `default feature` additionally requires that
# crate's `default` list to enable it.
feature_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "FEAT" { print $2 "\t" $3 "\t" $4 "\t" $5 }')"
if [ -n "$feature_claims" ]; then
    declared_features="$(printf '%s' "$metadata" | jq -r '
        .packages[] | . as $p
        | ($p.features.default // []) as $default
        | ($p.features | keys[])
        | "\($p.name)\t\(.)\t\(if . as $f | $default | index($f) then "default" else "off" end)"
    ')"
    feature_errors="$(
        printf '%s\n' "$feature_claims" | while IFS="$(printf '\t')" read -r subject qual names owners; do
            for feature in $names; do
                found=""
                for owner in $owners; do
                    state="$(printf '%s\n' "$declared_features" |
                        awk -F'\t' -v o="$owner" -v f="$feature" '$1 == o && $2 == f { print $3 }')"
                    if [ "$qual" = "default" ]; then
                        [ "$state" = "default" ] && found="$owner"
                    else
                        [ -n "$state" ] && found="$owner"
                    fi
                    [ -n "$found" ] && break
                done
                if [ -z "$found" ]; then
                    if [ "$qual" = "default" ]; then
                        echo "  $subject: no crate it names enables '$feature' by default"
                    else
                        echo "  $subject: no crate it names declares a '$feature' feature"
                    fi
                fi
            done
        done
        true
    )"
    if [ -n "$feature_errors" ]; then
        fail=1
        echo "check-readme-crates: feature claims in $readme that cargo metadata refutes:"
        echo "$feature_errors"
        echo "  Write a feature as: Cargo feature \`name\` -- or default Cargo feature \`name\` --"
        echo "  and name the crate that declares it in the same row. The marker carries 'Cargo'"
        echo "  because bare 'feature'/'features' is an ordinary English verb."
    fi
fi

# `external dependency`/`external dependencies`: each named crate must be a non-dev dependency of
# one of the workspace crates the text names. Under a marker the writer has said that this code
# span is a crate name, which is what the `gamut`-prefixed cite check cannot decide on its own.
extdep_claims="$(printf '%s\n' "$scan" | awk -F'\t' '$1 == "EXTDEP" { print $2 "\t" $3 "\t" $4 }')"
if [ -n "$extdep_claims" ]; then
    actual_deps="$(printf '%s' "$metadata" | jq -r '
        .packages[]
        | "\(.name)\t\([ .dependencies[] | select((.kind // "normal") != "dev") | .name ]
             | unique | sort | join(" "))"
    ')"
    extdep_errors="$(
        printf '%s\n' "$extdep_claims" | while IFS="$(printf '\t')" read -r subject names owners; do
            for dep in $names; do
                found=""
                for owner in $owners; do
                    if printf '%s\n' "$actual_deps" |
                        awk -F'\t' -v o="$owner" -v d="$dep" '
                            $1 == o { n = split($2, have, " ")
                                      for (i = 1; i <= n; i++) { if (have[i] == d) { found = 1 } } }
                            END { exit found ? 0 : 1 }'; then
                        found="$owner"
                        break
                    fi
                done
                if [ -z "$found" ]; then
                    echo "  $subject: no crate it names depends on '$dep'"
                fi
            done
        done
        true
    )"
    if [ -n "$extdep_errors" ]; then
        fail=1
        echo "check-readme-crates: external-dependency claims in $readme that cargo metadata refutes:"
        echo "$extdep_errors"
        echo "  Write one as: external dependency \`name\` -- and name, in the same row, a crate"
        echo "  that declares it. Dev-dependencies are excluded, as they are everywhere here."
    fi
fi

if [ "$fail" -eq 0 ]; then
    echo "README crates table lists every workspace crate ($(echo "$workspace_crates" | wc -l) crates)"
fi

exit "$fail"
