#!/usr/bin/env bash
# The regression battery for `run.sh`, the AGENTS.md dependency-edge guard beside this file.
#
# Each fixture is ONE edit to the repository's AGENTS.md, applied to a copy, and asserts the exit
# code the guard must return and, where the reason matters, a fragment it must print -- so a
# fixture that starts failing for a different reason is caught rather than counted as a pass.
# Legal renderings are fixtures too, with expected code 0: a guard that rejects legal prose is a
# guard someone turns off.
#
# Usage:
#     tooling/check-agents-edges/fixtures.sh          # cut from the repository's AGENTS.md
#     tooling/check-agents-edges/fixtures.sh PATH     # cut from another file
set -uo pipefail

root="$(git rev-parse --show-toplevel)" || {
    echo "fixtures: not inside a git repository"
    exit 1
}
cd "$root" || exit 1

guard="./tooling/check-agents-edges/run.sh"
base="${1:-AGENTS.md}"
test -f "$base" || {
    echo "fixtures: no such file: $base"
    exit 1
}

work="$(mktemp -d)" || exit 1
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# check <name> <expected exit> <expected output fragment, or ""> <transform...>
check() {
    name="$1"
    want="$2"
    fragment="$3"
    shift 3

    "$@" <"$base" >"$work/AGENTS.md" || {
        echo "FAIL  $name -- the fixture transform itself failed"
        failed=$((failed + 1))
        return
    }
    out="$("$guard" "$work/AGENTS.md" 2>&1)"
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

unchanged() { cat; }
sedx() { sed "$1"; }

# The base must pass: every fixture is a single edit away from it.
check "base passes" 0 "agree with cargo metadata" unchanged

# Legal renderings.
check "CRLF line endings are read" 0 "" sedx 's/$/\r/'
check "a note parenthesis is not an edge kind" 0 "" \
    sedx 's/^- \*\*gamut-bitstream\*\* — \(.*\) ← core\./- **gamut-bitstream** — \1 ← core (the AV1 coder, §8.2; see lib.rs)./'

# Edges.
check "a missing edge is reported" 1 \
    "gamut-webp: cargo metadata has an edge to gamut-dsp (normal) that AGENTS.md does not list" \
    sedx 's/← core, codec-abi, color, dsp, riff\./← core, codec-abi, color, riff./'
check "a phantom edge is reported" 1 \
    "gamut-riff: AGENTS.md lists an edge to gamut-bitstream (normal) that cargo metadata does not have" \
    sedx 's/^\(- \*\*gamut-isobmff\*\* .*\) ← core\./\1 ← core, bitstream./'
check "an edge shared by a bullet's heads is checked for each head" 1 \
    "gamut-isobmff: AGENTS.md lists an edge to gamut-bitstream (normal)" \
    sedx 's/^\(- \*\*gamut-isobmff\*\* .*\) ← core\./\1 ← core, bitstream./'
check "an optional edge written as normal is reported" 1 \
    "gamut-heic: cargo metadata has an edge to gamut-metadata (optional)" \
    sedx '/gamut-heic/,/gamut-deflate/s/metadata (optional,/metadata (always,/'
check "a build edge written as normal is reported" 1 \
    "gamut-jxl-sys: cargo metadata has an edge to cmake (build)" \
    sedx 's/← `cmake` (build)/← `cmake`/'
check "an external edge is held to the graph too" 1 \
    "gamut-cli: cargo metadata has an edge to clap (normal)" \
    sedx 's/← gamut, `clap`, /← gamut, /'
check "the clause ends at the first full stop" 1 \
    "gamut-tonemap: cargo metadata has an edge to gamut-core (normal)" \
    sedx '/gamut-tonemap/,/gamut-codec-abi/s/re-encode\. ← core\./re-encode. ←. core./'

# Grammar.
check "a bullet with two clauses is rejected" 1 "exactly one" \
    sedx 's/^- \*\*gamut-wasm\*\* (cdylib)\. ← gamut, `wasm-bindgen`\./& ← gamut./'
check "nothing beside another item is rejected" 1 "\`nothing\` must be the only item" \
    sedx 's/^\(- \*\*gamut-dsp\*\* .*\) ← nothing\./\1 ← nothing, core./'
check "an unrecognised item is rejected" 1 "unrecognised edge item" \
    sedx 's/^- \*\*gamut-ffi\*\* (cdylib\/staticlib)\. ← gamut, codec-abi, color\./- **gamut-ffi** (cdylib\/staticlib). ← gamut, codec-abi, Color Science./'
check "a prefixed workspace name is rejected" 1 "without its \`gamut-\` prefix" \
    sedx 's/← gamut, codec-abi, color\./← gamut, gamut-codec-abi, color./'

# Membership.
check "a member without a bullet is reported" 1 "gamut-wasm is a workspace member with no bullet" \
    sedx '/^- \*\*gamut-wasm\*\*/d'
check "a phantom bullet is reported" 1 "gamut-phantom opens a bullet but is not a workspace member" \
    sedx 's/^- \*\*gamut-wasm\*\* (cdylib)\. ← gamut, `wasm-bindgen`\./&\n- **gamut-phantom** — gone. ← core./'
check "a crate stated twice is reported" 1 "gamut-wasm opens more than one bullet" \
    sedx 's/^- \*\*gamut-wasm\*\* (cdylib)\. ← gamut, `wasm-bindgen`\./&\n&/'
check "a bullet without a bold head is reported" 1 "does not open with a bold crate name" \
    sedx 's/^- \*\*gamut-wasm\*\* (cdylib)\. ← gamut, `wasm-bindgen`\./&\n- gamut-loose. ← core./'

echo "check-agents-edges fixtures: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
