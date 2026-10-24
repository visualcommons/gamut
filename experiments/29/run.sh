#!/usr/bin/env bash
# Experiment #29: quantization choices gamut-jpeg's encoder does not make yet -- alternate
# luminance tables, the luma/chroma balance, XYB-directed tables -- against the shipped Annex K
# path and the shipped trellis modes, as rate at equal fidelity.
#
# Regenerates everything under experiments/29/ except RESULTS.md (which reads it):
#   raw/measurements.csv  -- one row per image x variant x quality, from
#                            crates/gamut-jpeg/examples/experiment_29.rs (columns in its module doc)
#   raw/tables.csv        -- the alternate base tables the arm builds, natural order
#   raw/bdrate-vs-annexk.csv, raw/bdrate-vs-xyb-annexk.csv
#                         -- per image x variant x metric rate difference against the named
#                            baseline, plus the mean over images (the arm's `bdrate` subcommand)
#   run.json              -- provenance: git SHA, rustc, lockfile hashes, input SHA-256s, CPU
#
# One build of the tree as checked out: every variant is a configuration of the public
# `JpegEncoder`, so nothing is patched. Every stream is decoded by libjpeg-turbo (the crate's
# oracle). Encodes are deterministic (the XYB path within gamut-color's f64 Tier-1 determinism),
# so there is one sample per point and no timing.
#
# The real-content inputs are the libjpeg-turbo test images (`third_party/libjpeg-turbo`
# submodule, which the oracle build needs anyway); the synthetic one is the decode bench's
# deterministic generator. EXPERIMENT_SYNTHETIC (default 512x384) sets its size.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/29
raw=$exp/raw
work=target/experiments/29
images=third_party/libjpeg-turbo/testimages
synthetic=${EXPERIMENT_SYNTHETIC:-512x384}
inputs=(
  "testorig=$images/testorig.ppm"
  "shira_bird8=$images/shira_bird8.bmp"
  "vgl_6434_0018a=$images/vgl_6434_0018a.bmp"
  "vgl_6548_0026a=$images/vgl_6548_0026a.bmp"
)
for i in "${inputs[@]}"; do
  [ -f "${i#*=}" ] || {
    echo "refusing: ${i#*=} is missing; run \`git submodule update --init third_party/libjpeg-turbo\`" >&2
    exit 1
  }
done

mkdir -p "$work"
rm -f "$work"/*.csv
head_start=$(git rev-parse HEAD)
dirty_start=$(git diff --quiet HEAD && echo false || echo true)

# The run writes under target/ first: a failed build, a failed encode or decode, or a moved HEAD
# then leaves the committed raw/ untouched, and raw/ changes only alongside run.json.
cargo build --release --quiet -p gamut-jpeg --example experiment_29
arm=target/release/examples/experiment_29
image_args=()
for i in "${inputs[@]}"; do image_args+=(--image "$i"); done
"$arm" measure "${image_args[@]}" --synthetic "$synthetic" >"$work/measurements.csv"
"$arm" tables >"$work/tables.csv"
for baseline in annexk xyb-annexk; do
  "$arm" bdrate --baseline "$baseline" "$work/measurements.csv" >"$work/bdrate-vs-$baseline.csv"
done

# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work"/measurements.csv "$work"/tables.csv "$work"/bdrate-vs-*.csv "$raw/"

sha() { sha256sum "$1" | cut -d' ' -f1; }
json_str() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
inputs_json=$(
  first=1
  for i in "${inputs[@]}"; do
    [ $first -eq 1 ] || printf ','
    first=0
    printf '\n    %s: "%s"' "$(json_str "${i#*=}")" "$(sha "${i#*=}")"
  done
)
mise_lock=null
[ -f mise.lock ] && mise_lock="\"$(sha mise.lock)\""

cat >"$exp/run.json" <<EOF
{
  "issue": 29,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "libjpeg_turbo_submodule": "$(git rev-parse HEAD:third_party/libjpeg-turbo)",
  "rustc": $(json_str "$(rustc -vV | tr '\n' ';')"),
  "mise_lock_sha256": $mise_lock,
  "cargo_lock_sha256": "$(sha Cargo.lock)",
  "rust_toolchain_toml_sha256": "$(sha rust-toolchain.toml)",
  "synthetic": "$synthetic",
  "seeds": "none: encodes are deterministic and the synthetic input is a fixed-seed LCG generator (crates/gamut-jpeg/benches/common/corpus.rs)",
  "input_sha256": {$inputs_json
  },
  "cpu_model": $(json_str "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"),
  "cpu_cores": $(nproc)
}
EOF

# Summary on stdout: the mean rate difference per variant and metric against each baseline.
for baseline in annexk xyb-annexk; do
  echo "vs $baseline (mean BD-rate %, negative is smaller at equal fidelity)"
  awk -F, '$1 == "mean" { printf "  %-20s %-10s %8s\n", $2, $4, $5 }' "$raw/bdrate-vs-$baseline.csv"
done
