#!/usr/bin/env bash
# Experiment #305: gamut-jpeg's decode throughput against zune-jpeg, before and after the decoder
# work this issue delivered, with libjpeg-turbo as the output reference.
#
# Regenerates everything under experiments/305/ except RESULTS.md (which reads it):
#   raw/before.csv, raw/head.csv
#                -- one row per input x decoder x sample, from
#                   crates/gamut-jpeg/examples/experiment_305.rs (columns in its module doc)
#   run.json     -- provenance: git SHA, the pre-optimisation SHA, rustc, lockfile hashes, corpus
#                   SHA-256s, CPU, load average at start and end
#
# `head` is the tree as checked out. `before` is the same tree with crates/gamut-jpeg/src swapped
# back to its state at BEFORE_SHA (the default branch the work started from) for the duration of
# one build -- the diff is taken by git at run time and reverted on any exit, so every other file
# (the arm, the corpus, the dependencies) is the one `head` uses and the two variants differ in the
# decoder alone. The script refuses to run if that directory carries uncommitted changes, so it
# never measures, or reverts over, anything but its own swap. Both variants run in one session, and
# both measure zune-jpeg and libjpeg-turbo too, so every ratio is taken against a reference timed
# minutes earlier on the same machine.
#
# EXPERIMENT_SAMPLES (default 100) sets the timed decodes per input x decoder; each also gets one
# untimed warm-up decode. EXPERIMENT_WIDTH x EXPERIMENT_HEIGHT (default 6000 x 4000, 24 MP: the
# small end of the issue's 24-45 MP camera range) sets the image size.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/305
raw=$exp/raw
work=target/experiments/305
site=crates/gamut-jpeg/src
before_sha=63e0762aeb97c7eb8e25d0281d17735cfbccdcf8
samples=${EXPERIMENT_SAMPLES:-100}
width=${EXPERIMENT_WIDTH:-6000}
height=${EXPERIMENT_HEIGHT:-4000}

git diff --quiet HEAD -- "$site" || {
  echo "refusing: $site has uncommitted changes; commit or set them aside first" >&2
  exit 1
}
git cat-file -e "$before_sha^{commit}" || {
  echo "refusing: $before_sha is not in this clone; fetch the default branch first" >&2
  exit 1
}

mkdir -p "$work/corpus"
rm -f "$work"/corpus/*.jpg "$work"/*.csv "$work"/before.patch
git diff --binary HEAD "$before_sha" -- "$site" >"$work/before.patch"
load_start=$(cut -d' ' -f1-3 /proc/loadavg)
head_start=$(git rev-parse HEAD)
dirty_start=$(git diff --quiet HEAD && echo false || echo true)

# Each run writes under target/ first: a failed build, a failed decode or a moved HEAD then leaves
# the committed raw/*.csv untouched, and raw/ changes only alongside run.json.
measure() {
  cargo run --release --quiet -p gamut-jpeg --example experiment_305 -- \
    --variant "$1" --samples "$samples" --width "$width" --height "$height" \
    --corpus-dir "$work/corpus" >"$work/$1.csv"
}

if [ -s "$work/before.patch" ]; then
  git apply "$work/before.patch"
  trap 'git apply -R "$work/before.patch"' EXIT
  measure before
  git apply -R "$work/before.patch"
  trap - EXIT
else
  measure before
fi
measure head

load_end=$(cut -d' ' -f1-3 /proc/loadavg)
# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work/before.csv" "$work/head.csv" "$raw/"

sha() { sha256sum "$1" | cut -d' ' -f1; }
json_str() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
corpus_json=$(
  first=1
  for f in "$work"/corpus/*.jpg; do
    [ $first -eq 1 ] || printf ','
    first=0
    printf '\n    %s: "%s"' "$(json_str "$(basename "$f")")" "$(sha "$f")"
  done
)
mise_lock=null
[ -f mise.lock ] && mise_lock="\"$(sha mise.lock)\""

cat >"$exp/run.json" <<EOF
{
  "issue": 305,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "before_sha": "$before_sha",
  "before_patch_sha256": "$(sha "$work/before.patch")",
  "rustc": $(json_str "$(rustc -vV | tr '\n' ';')"),
  "mise_lock_sha256": $mise_lock,
  "cargo_lock_sha256": "$(sha Cargo.lock)",
  "rust_toolchain_toml_sha256": "$(sha rust-toolchain.toml)",
  "width": $width,
  "height": $height,
  "samples_per_input_and_decoder": $samples,
  "warmup_decodes_per_input_and_decoder": 1,
  "seeds": "none: the corpus is a deterministic generator (fixed LCG seed) in crates/gamut-jpeg/benches/common/corpus.rs, encoded by libjpeg-turbo at q90",
  "corpus_sha256": {$corpus_json
  },
  "cpu_model": $(json_str "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"),
  "cpu_cores": $(nproc),
  "load_average_start": "$load_start",
  "load_average_end": "$load_end"
}
EOF

# Summary on stdout, per input: median decode time per variant x decoder in milliseconds, gamut's
# ratio to zune-jpeg within each variant's session, and gamut's max difference from libjpeg-turbo.
awk -F, '
  FNR == 1 { next }
  {
    run = FILENAME; sub(/.*\//, "", run); sub(/\.csv$/, "", run)
    key = $1; d = run "/" $6
    n[key, d]++; t[key, d, n[key, d]] = $8; maxd[key, d] = $10
    if (!(key in seen)) { seen[key] = 1; order[++rows] = key }
  }
  function median(k, d,   m, i, j, a, tmp) {
    m = n[k, d]
    for (i = 1; i <= m; i++) a[i] = t[k, d, i]
    for (i = 1; i <= m; i++) for (j = i + 1; j <= m; j++) if (a[j] < a[i]) { tmp = a[i]; a[i] = a[j]; a[j] = tmp }
    return a[int((m + 1) / 2)]
  }
  END {
    printf "%-16s %10s %10s %10s %10s %8s %8s %6s\n", "input", "before_ms", "head_ms", "zune_ms", "turbo_ms", "before_x", "head_x", "maxd"
    for (r = 1; r <= rows; r++) {
      k = order[r]
      b = median(k, "before/gamut"); h = median(k, "head/gamut"); z = median(k, "head/zune-jpeg")
      zb = median(k, "before/zune-jpeg")
      printf "%-16s %10.1f %10.1f %10.1f %10.1f %8.2f %8.2f %6s\n", k, b / 1e6, h / 1e6, z / 1e6,
        median(k, "head/libjpeg-turbo") / 1e6, b / zb, h / z, maxd[k, "head/gamut"]
    }
  }
' "$raw/before.csv" "$raw/head.csv"
