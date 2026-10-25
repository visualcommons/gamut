#!/usr/bin/env bash
# Experiment #30: encoder techniques from jpegli and libjpeg-turbo that gamut-jpeg does not use and
# that #29/#974 did not measure, each as a per-build patch of crates/gamut-jpeg/src, measured
# against the tree as checked out.
#
# Regenerates everything under experiments/30/ except RESULTS.md (which reads it):
#   raw/inputs.csv          -- #974's generated inputs and their share of white pixels
#                              (crates/gamut-jpeg/examples/experiment_974.rs)
#   raw/baseline.csv, raw/<quant patch>.csv
#                           -- one row per image x variant x quality, from #29's arm
#                              crates/gamut-jpeg/examples/experiment_29.rs (columns in its module
#                              doc), for the tree as checked out and for each quantization patch
#   raw/bdrate-<quant patch>.csv
#                           -- per image x variant x metric rate difference of the patched build's
#                              variant against the same variant in the baseline build, plus the
#                              mean over images (#29's `bdrate`); `variant` reads `<patch>:<variant>`
#   raw/bdrate-zero-bias-vs-trellis.csv
#                           -- the same, for the zero-biased plain variants against the baseline
#                              build's trellis variant on the same tables (the two alternatives)
#   raw/changed.csv         -- per quantization patch x variant x image, how many quality points
#                              coded to a different size than the baseline build's
#   raw/scans-baseline.csv, raw/<scan patch>.csv
#                           -- one row per image x configuration x quality, from
#                              crates/gamut-jpeg/examples/experiment_30.rs (columns in its module
#                              doc), for the shipped scan script and for each scan-script patch
#   raw/scans-summary.csv   -- per script x configuration x image, the mean over qualities of the
#                              progressive size against the shipped script's and against the
#                              sequential stream, in percent, plus the mean over images
#   run.json                -- provenance: git SHA, rustc, lockfile hashes, patch hashes, input
#                              SHA-256s, CPU
#
# Quantization patches (measured with #29's arm, every one of its variants):
#   zero-bias        -- jpegli's non-adaptive YCbCr AC zero bias (encoder.rs, progressive.rs)
#   downsample-bias  -- libjpeg-turbo's alternating 1,2 rounding bias in the 2:1 box (encoder.rs)
#   smooth-10        -- libjpeg-turbo's input smoothing at smoothing_factor 10 (encoder.rs)
# Scan-script patches (measured with #30's arm; coefficients unchanged, so rate only):
#   scans-mozjpeg    -- mozjpeg's JCP_MAX_COMPRESSION script (jpgcrush's) (progressive.rs)
#   scans-jpegli     -- jpegli's default progressive_level 2 script (progressive.rs)
# Each patch is applied for the duration of one build and reverted on any exit. The script refuses
# to run if crates/gamut-jpeg/src already carries uncommitted changes, so it never measures, or
# reverts over, anything but its own patch.
#
# Every stream is decoded by libjpeg-turbo (the crate's oracle). Encodes are deterministic, so there
# is one sample per point and no timing. Inputs: #29's corpus (the libjpeg-turbo test images and the
# decode bench's synthetic generator; EXPERIMENT_SYNTHETIC, default 512x384, sets its size) plus
# the two saturated images #974's arm writes.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/30
raw=$exp/raw
work=target/experiments/30
images=third_party/libjpeg-turbo/testimages
synthetic=${EXPERIMENT_SYNTHETIC:-512x384}
quant_patches=(zero-bias downsample-bias smooth-10)
scan_patches=(scans-mozjpeg scans-jpegli)
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
for p in "${quant_patches[@]}" "${scan_patches[@]}"; do git apply --check "$exp/$p.patch"; done

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
measure_quant() {
  cargo build --release --quiet -p gamut-jpeg --example experiment_29
  target/release/examples/experiment_29 measure "${image_args[@]}" --synthetic "$synthetic" \
    >"$work/$1.csv"
}
measure_scans() {
  cargo build --release --quiet -p gamut-jpeg --example experiment_30
  target/release/examples/experiment_30 scans "${image_args[@]}" --synthetic "$synthetic" \
    >"$work/$1.csv"
}

measure_quant baseline
measure_scans scans-baseline
for p in "${quant_patches[@]}" "${scan_patches[@]}"; do
  git apply "$exp/$p.patch"
  trap 'git apply -R "$exp/$p.patch"' EXIT
  case $p in
    scans-*) measure_scans "$p" ;;
    *) measure_quant "$p" ;;
  esac
  git apply -R "$exp/$p.patch"
  trap - EXIT
done
# The comparison runs on the baseline build of #29's arm, so `bdrate` is the tree's own.
cargo build --release --quiet -p gamut-jpeg --example experiment_29
arm=target/release/examples/experiment_29

variants=$(awk -F, 'NR > 1 && !seen[$4]++ { print $4 }' "$work/baseline.csv")
for p in "${quant_patches[@]}"; do
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
# The zero bias and the shipped trellis are alternatives on one path (the patch leaves the RD path
# alone), so the zero-biased plain variant is also compared with the baseline build's trellis
# variant on the same tables: `zero-bias:annexk` against `trellis`, `zero-bias:luma-csf32` against
# `luma-csf32-trellis`.
out="$work/bdrate-zero-bias-vs-trellis.csv"
echo "image,variant,baseline,metric,bd_rate_pct,overlap_lo,overlap_hi" >"$out"
for vt in annexk:trellis luma-csf32:luma-csf32-trellis; do
  v=${vt%%:*} t=${vt#*:}
  pair="$work/pair.csv"
  {
    head -n1 "$work/baseline.csv"
    awk -F, -v t="$t" 'NR > 1 && $4 == t' "$work/baseline.csv"
    awk -F, -v v="$v" 'BEGIN { OFS = "," } NR > 1 && $4 == v { $4 = "zero-bias:" v; print }' \
      "$work/zero-bias.csv"
  } >"$pair"
  "$arm" bdrate --baseline "$t" "$pair" | tail -n +2 >>"$out"
done
rm -f "$work/pair.csv"
{
  echo "build,variant,image,points_changed,points"
  for p in "${quant_patches[@]}"; do
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
# Per script x configuration x image: mean over qualities of 100 (bytes / reference - 1), against
# the shipped script's progressive stream (`vs_shipped_pct`) and against the sequential stream
# with optimized tables (`vs_sequential_pct`); the shipped script's own row has vs_shipped 0.
{
  echo "script,config,image,vs_shipped_pct,vs_sequential_pct,scans"
  for s in scans-baseline "${scan_patches[@]}"; do
    awk -F, -v s="$s" '
      FNR == 1 { next }
      NR == FNR { ref[$1, $4, $5] = $7; next }
      {
        k = $4 SUBSEP $1; if (!(k in n)) order[++rows] = k
        n[k]++; shipped[k] += 100 * ($7 / ref[$1, $4, $5] - 1); seq[k] += 100 * ($7 / $6 - 1)
        nscans[k] = $8
      }
      END {
        for (r = 1; r <= rows; r++) {
          k = order[r]; split(k, f, SUBSEP)
          printf "%s,%s,%s,%.2f,%.2f,%d\n", s, f[1], f[2], shipped[k] / n[k], seq[k] / n[k], nscans[k]
          c = f[1]; if (!(c in cn)) corder[++crows] = c
          cn[c]++; cshipped[c] += shipped[k] / n[k]; cseq[c] += seq[k] / n[k]
        }
        for (r = 1; r <= crows; r++) {
          c = corder[r]
          printf "%s,%s,mean,%.2f,%.2f,\n", s, c, cshipped[c] / cn[c], cseq[c] / cn[c]
        }
      }
    ' "$work/scans-baseline.csv" "$work/$s.csv"
  done
} >"$work/scans-summary.csv"

# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work"/inputs.csv "$work"/baseline.csv "$work"/changed.csv "$work"/scans-baseline.csv \
  "$work"/scans-summary.csv "$work"/bdrate-zero-bias-vs-trellis.csv "$raw/"
for p in "${quant_patches[@]}"; do cp "$work/$p.csv" "$work/bdrate-$p.csv" "$raw/"; done
for p in "${scan_patches[@]}"; do cp "$work/$p.csv" "$raw/"; done

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
patches_json=$(
  first=1
  for p in "${quant_patches[@]}" "${scan_patches[@]}"; do
    [ $first -eq 1 ] || printf ','
    first=0
    printf '\n    "%s": "%s"' "$p" "$(sha "$exp/$p.patch")"
  done
)
mise_lock=null
[ -f mise.lock ] && mise_lock="\"$(sha mise.lock)\""

cat >"$exp/run.json" <<EOF
{
  "issue": 30,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "variant_patch_sha256": {$patches_json
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

# Summary on stdout: per quantization patch, the mean rate difference of every variant against the
# same variant unpatched and how many points it changed; then the scan-script table.
for p in "${quant_patches[@]}"; do
  echo "$p vs baseline build (mean BD-rate %, negative is smaller at equal fidelity)"
  awk -F, '$1 == "mean" { printf "  %-36s %-10s %8s\n", $2, $4, $5 }' "$raw/bdrate-$p.csv"
  echo "$p: quality points coded to a different size, per variant (over all images)"
  awk -F, -v p="$p" '$1 == p { c[$2] += $4; n[$2] += $5; if (!($2 in s)) { s[$2] = 1; o[++r] = $2 } }
    END { for (i = 1; i <= r; i++) printf "  %-24s %4d / %4d\n", o[i], c[o[i]], n[o[i]] }' "$raw/changed.csv"
done
echo "zero-bias plain variants vs the baseline build's trellis on the same tables (mean BD-rate %)"
awk -F, '$1 == "mean" { printf "  %-36s %-20s %-10s %8s\n", $2, $3, $4, $5 }' \
  "$raw/bdrate-zero-bias-vs-trellis.csv"
echo "scan scripts: mean size % vs the shipped script and vs sequential (negative is smaller)"
awk -F, '$3 == "mean" { printf "  %-16s %-10s %8s %8s\n", $1, $2, $4, $5 }' "$raw/scans-summary.csv"
