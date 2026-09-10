#!/usr/bin/env python3
"""Extract the IIM 4.2 DataSet NAME column from the standard's own PDF.

Run from the workspace root, with `pdftotext` (poppler-utils) on PATH:

    python3 crates/gamut-iptc/tests/data/extract-iim-names.py references/iptc/iim-4.2.pdf \
        > crates/gamut-iptc/tests/data/iim-4.2-dataset-names.tsv

`pdftotext` is a system package the workspace toolchain does not provision, so the result is
committed as a derived artefact and `iim.rs` pins `KNOWN_TAGS` against it. Re-run this after
changing the vendored PDF; never hand-edit the table it produces.

IIM 4.2 lays every DataSet out as three columns: the `record:dataset` number at the left margin,
the DataSet name beside it, and the type/length statement and description to the right.
`pdftotext -bbox-layout` keeps each word's position on the page, so the name column is recovered
by geometry rather than by guessing where wrapped plain text belongs. A name set over several
lines is rejoined from the name-column lines that follow it, across a page break if need be; a
trailing '-' or '/' joins without a space, and a '-' the typesetter inserted only to hyphenate is
dropped when the standard's own index spells the name without it.
"""
import html
import re
import subprocess
import sys

ROW = re.compile(r"^(\d+):(\d+)$")
BODY = (60.0, 725.0)       # the page body, excluding the running header and footer
NUMBER_COL = 72.0          # left margin: the `record:dataset` column
NAME_COL = (110.0, 180.0)  # the DataSet name column: no name word starts right of 167
SAME_LINE = 1.5            # baselines this close are one typeset line
SKIP = 2                   # description lines a name may be set around before it has ended


def body_lines(pdf):
    """Every body line of the document in reading order, as (page, [(x, word), ...])."""
    xml = subprocess.run(["pdftotext", "-bbox-layout", pdf, "-"],
                         check=True, capture_output=True, text=True).stdout
    for page, text in enumerate(re.split(r"(?=<page )", xml)[1:]):
        words = sorted((float(y), float(x), html.unescape(t)) for x, y, t in
                       re.findall(r'<word xMin="([\d.]+)" yMin="([\d.]+)"[^>]*>([^<]*)</word>',
                                  text))
        line, base = [], None
        for y, x, t in words:
            if base is not None and y - base > SAME_LINE:
                yield page, base, sorted(line)
                line = []
            base = y if not line else base
            line.append((x, t))
        if line:
            yield page, base, sorted(line)


def cell(words):
    return [t for x, t in words if NAME_COL[0] <= x < NAME_COL[1]]


def rows(lines):
    """Each DataSet row as ((record, dataset), [name words]).

    A row's number sits alone at the left margin with its name beside it; running text that merely
    begins with a `record:dataset` reference flows on through the gutter between the two columns,
    and Appendix F's table header carries another number where the name belongs.
    """
    for i, (_, _, words) in enumerate(lines):
        if not words or abs(words[0][0] - NUMBER_COL) > 1.0 or not ROW.match(words[0][1]):
            continue
        if any(NUMBER_COL + 18 < x < NAME_COL[0] for x, _ in words):
            continue
        name = cell(words)
        if not name or ROW.match(name[0]):
            continue
        skipped = 0
        for page, _, more in lines[i + 1:]:
            if abs(more[0][0] - NUMBER_COL) <= 1.0:
                break
            rest = cell(more)
            if rest:
                name += rest
                skipped = 0
            else:
                skipped += 1
                if skipped > SKIP:
                    break
        yield ROW.match(words[0][1]).groups(), name


def join(parts, index):
    out = parts[0]
    for part in parts[1:]:
        out = out + part if out.endswith(("-", "/")) else out + " " + part
    if "-" not in out or out in index:
        return out
    hits = [v for v in (out[:i] + out[i + 1:] for i, c in enumerate(out) if c == "-")
            if v in index]
    if len(hits) != 1:
        raise SystemExit(f"cannot resolve the hyphen in {out!r} against the index")
    return hits[0]


def main(pdf):
    lines = [(p, y, w) for p, y, w in body_lines(pdf) if BODY[0] <= y <= BODY[1]]
    index, seen = set(), False
    for _, _, words in lines:  # the index spells every wrapped name out on one line
        seen = seen or any(t == "INDEX" for _, t in words)
        if seen:
            index.add(" ".join(t for x, t in words if x < 400))
    named = {}
    for (record, dataset), parts in rows(lines):
        key, name = (int(record), int(dataset)), join(parts, index)
        if named.setdefault(key, name) != name:
            raise SystemExit(f"{key} is named both {named[key]!r} and {name!r}")
    print("# IIM 4.2 DataSet names, derived from references/iptc/iim-4.2.pdf. Never hand-edit:\n"
          "# regenerate with the extract-iim-names.py beside this file, as its docstring records.\n"
          "# Columns: record, dataset, and the DataSet's name as the standard's NAME column sets it.")
    for (record, dataset), name in sorted(named.items()):
        print(f"{record}\t{dataset}\t{name}")


main(sys.argv[1])
