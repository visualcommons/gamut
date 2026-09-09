#!/usr/bin/env bash
# The README's "## Crates" table is hand-maintained prose, and nothing read it. By issue #425 it
# had drifted far enough that eighteen rows needed correcting -- five shipped crates were still
# described as unstarted scaffolding citing a closed issue -- and four crates (gamut-codec-abi,
# gamut-dng, gamut-jpeg, gamut-tonemap) had no row at all.
#
# This guard checks the one property that is mechanical -- the table's *membership* -- and
# nothing else:
#
#   * every workspace crate has a row, so a new crate cannot be added without documenting it;
#   * every row names a crate that still exists, so a renamed or deleted crate cannot be left
#     behind as a phantom row.
#
# What is deliberately NOT checked, and why:
#   * The Purpose and Status cells. They are prose a human maintains, and their authority is the
#     crate's own STATUS.md. A text gate over them would fossilise a particular wording, and a
#     generated table would move prose a human writes into a generator -- so staleness of a row's
#     *text* stays a review concern, not a lint. Membership is what a machine can settle.
#   * The version. `mise run versions` already reports it, and the README deliberately states
#     what each crate *is* rather than pinning a number that release-plz bumps.
set -euo pipefail

readme="${1:-README.md}"

test -f "$readme" || {
    echo "check-readme-crates: no such file: $readme"
    exit 1
}

# The rows of the "## Crates" table only. Bounded to that section so the README's other tables
# (the `mise run ...` command table) can never be mistaken for a crate row.
readme_crates="$(
    awk '/^## Crates$/ { in_section = 1; next } /^## / { in_section = 0 } in_section' "$readme" |
        sed -nE 's/^\| *`([a-z0-9-]+)` *\|.*/\1/p' | sort -u
)"

test -n "$readme_crates" || {
    echo "check-readme-crates: found no crate rows under '## Crates' in $readme"
    exit 1
}

# The same source of truth `mise run versions` reads: --no-deps lists workspace members only.
workspace_crates="$(
    cargo metadata --no-deps --format-version 1 | jq -r '.packages | sort_by(.name)[] | .name'
)"

fail=0

missing="$(comm -23 <(echo "$workspace_crates") <(echo "$readme_crates"))"
if [ -n "$missing" ]; then
    fail=1
    echo "check-readme-crates: workspace crates with no row in the $readme crates table:"
    echo "$missing" | sed 's/^/  /'
    echo "  add a row (Crate | Purpose | Status), taking the status from the crate's STATUS.md."
fi

phantom="$(comm -13 <(echo "$workspace_crates") <(echo "$readme_crates"))"
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
