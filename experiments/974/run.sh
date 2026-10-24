#!/usr/bin/env bash
# Experiment #974: two encoder-internal techniques #29 could not reach through the public API --
# a DC trellis and mozjpeg-style overshoot deringing -- each as a per-build patch of
# crates/gamut-jpeg/src, measured as rate at equal fidelity against the tree as checked out.
#
# Regenerates everything under experiments/974/ except RESULTS.md (which reads it):
#   raw/inputs.csv    -- the generated inputs and their share of white pixels, from
#                        crates/gamut-jpeg/examples/experiment_974.rs
#   raw/<build>.csv   -- one row per image x variant x quality for each build (baseline, then each
#                        patch below), from #29's arm crates/gamut-jpeg/examples/experiment_29.rs
#                        (columns in its module doc)
#   raw/bdrate-<patch>.csv
#                     -- per image x variant x metric rate difference of the patched build's
#                        variant against the same variant in the baseline build, plus the mean over
#                        images (#29's `bdrate` subcommand); `variant` reads `<patch>:<variant>`
#   raw/changed.csv   -- per patch x variant x image, how many of the quality points coded to a
#                        different size than the baseline build's
#   run.json          -- provenance: git SHA, rustc, lockfile hashes, patch hashes, input SHA-256s,
#                        CPU
#
# The baseline is the tree as checked out. Each patch is applied for the duration of one build of
# #29's arm and reverted on any exit:
#   dc-trellis          -- crates/gamut-jpeg/src/{rd,encoder}.rs: with RD optimization on, each
#                          component's DC is chosen by a trellis over its coding-order blocks
#                          through the F.1.2.1 predictor (baseline process only)
#   overshoot-deringing -- crates/gamut-jpeg/src/encoder.rs: every block's saturated samples are
#                          raised past 255 before the FDCT (every variant, both processes)
# The script refuses to run if crates/gamut-jpeg/src already carries uncommitted changes, so it
# never measures, or reverts over, anything but its own patch.
#
# Every stream is decoded by libjpeg-turbo (the crate's oracle). Encodes are deterministic, so
# there is one sample per point and no timing. Inputs: #29's corpus (the libjpeg-turbo test images
# and the decode bench's synthetic generator; EXPERIMENT_SYNTHETIC, default 512x384, sets its size)
# plus the two saturated images experiment_974 writes.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/974
raw=$exp/raw
work=target/experiments/974
images=third_party/libjpeg-turbo/testimages
synthetic=${EXPERIMENT_SYNTHETIC:-512x384}
patches=(dc-trellis overshoot-deringing)
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
git diff --quiet HEAD -- crates/gamut-jpeg/src || {
  echo "refusing: crates/gamut-jpeg/src has uncommitted changes; commit or set them aside first" >&2
  exit 1
}
for p in "${patches[@]}"; do git apply --check "$exp/$p.patch"; done

mkdir -p "$work"
rm -rf "$work"/*.csv "$work/inputs"
head_start=$(git rev-parse HEAD)
dirty_start=$(git diff --quiet HEAD && echo false || echo true)

# Each run writes under target/ first: a failed build, a failed encode or decode, or a moved HEAD
# then leaves the committed raw/ untouched, and raw/ changes only alongside run.json.
cargo run --release --quiet -p gamut-jpeg --example experiment_974 -- \
  inputs --out "$work/inputs" >"$work/inputs.csv"
inputs+=(
  "clipped-512x384=$work/inputs/clipped-512x384.ppm"
  "text-384x256=$work/inputs/text-384x256.ppm"
)
image_args=()
for i in "${inputs[@]}"; do image_args+=(--image "$i"); done
measure() {
  cargo build --release --quiet -p gamut-jpeg --example experiment_29
  target/release/examples/experiment_29 measure "${image_args[@]}" --synthetic "$synthetic" \
    >"$work/$1.csv"
}

measure baseline
for p in "${patches[@]}"; do
  git apply "$exp/$p.patch"
  trap 'git apply -R "$exp/$p.patch"' EXIT
  measure "$p"
  git apply -R "$exp/$p.patch"
  trap - EXIT
done
# The comparison runs on the baseline build of the arm, so `bdrate` is the tree's own.
cargo build --release --quiet -p gamut-jpeg --example experiment_29
arm=target/release/examples/experiment_29

variants=$(awk -F, 'NR > 1 && !seen[$4]++ { print $4 }' "$work/baseline.csv")
for p in "${patches[@]}"; do
  out="$work/bdrate-$p.csv"
  echo "image,variant,baseline,metric,bd_rate_pct,overlap_lo,overlap_hi" >"$out"
  for v in $variants; do
    # The variant from both builds, the patched one renamed, so `bdrate` compares the two.
    pair="$work/pair.csv"
    {
      head -n1 "$work/baseline.csv"
      awk -F, -v v="$v" 'NR > 1 && $4 == v' "$work/baseline.csv"
      awk -F, -v v="$v" -v p="$p" 'BEGIN { OFS = "," } NR > 1 && $4 == v { $4 = p ":" v; print }' \
        "$work/$p.csv"
    } >"$pair"
    "$arm" bdrate --baseline "$v" "$pair" | tail -n +2 >>"$out"
  done
  rm -f "$work/pair.csv"
done
{
  echo "build,variant,image,points_changed,points"
  for p in "${patches[@]}"; do
    awk -F, -v p="$p" '
      FNR == 1 { next }
      NR == FNR { base[$1, $4, $5] = $6; next }
      {
        k = $1 SUBSEP $4; if (!(k in n)) order[++rows] = k
        n[k]++; if (base[$1, $4, $5] != $6) changed[k]++
      }
      END { for (r = 1; r <= rows; r++) { split(order[r], f, SUBSEP); printf "%s,%s,%s,%d,%d\n", p, f[2], f[1], changed[order[r]], n[order[r]] } }
    ' "$work/baseline.csv" "$work/$p.csv"
  done
} >"$work/changed.csv"

# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work"/inputs.csv "$work"/baseline.csv "$work"/changed.csv "$raw/"
for p in "${patches[@]}"; do cp "$work/$p.csv" "$work/bdrate-$p.csv" "$raw/"; done

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
  "issue": 974,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "variant_patch_sha256": {
    "dc-trellis": "$(sha "$exp/dc-trellis.patch")",
    "overshoot-deringing": "$(sha "$exp/overshoot-deringing.patch")"
  },
  "libjpeg_turbo_submodule": "$(git rev-parse HEAD:third_party/libjpeg-turbo)",
  "rustc": $(json_str "$(rustc -vV | tr '\n' ';')"),
  "mise_lock_sha256": $mise_lock,
  "cargo_lock_sha256": "$(sha Cargo.lock)",
  "rust_toolchain_toml_sha256": "$(sha rust-toolchain.toml)",
  "synthetic": "$synthetic",
  "seeds": "none: encodes are deterministic and the generated inputs are fixed-seed LCG generators (crates/gamut-jpeg/benches/common/corpus.rs, crates/gamut-jpeg/examples/experiment_974.rs)",
  "input_sha256": {$inputs_json
  },
  "cpu_model": $(json_str "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"),
  "cpu_cores": $(nproc)
}
EOF

# Summary on stdout: per patch, the mean rate difference of every variant it changed against the
# same variant unpatched, then how many points it changed per image.
for p in "${patches[@]}"; do
  echo "$p vs baseline build (mean BD-rate %, negative is smaller at equal fidelity)"
  awk -F, '$1 == "mean" { printf "  %-36s %-10s %8s\n", $2, $4, $5 }' "$raw/bdrate-$p.csv"
  echo "$p: quality points coded to a different size, per variant (over all images)"
  awk -F, -v p="$p" '$1 == p { c[$2] += $4; n[$2] += $5; if (!($2 in s)) { s[$2] = 1; o[++r] = $2 } }
    END { for (i = 1; i <= r; i++) printf "  %-24s %4d / %4d\n", o[i], c[o[i]], n[o[i]] }' "$raw/changed.csv"
done
