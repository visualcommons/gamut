#!/usr/bin/env bash
# Experiment #514: race an equal-raw-size chunk-free candidate in gamut-png, or keep declining it.
#
# Regenerates everything under experiments/514/ except RESULTS.md (which reads it):
#   raw/baseline.csv, raw/variant.csv -- one row per input x variant x sample, from
#                                        crates/gamut-png/examples/experiment_514.rs
#   run.json                          -- provenance: git SHA, rustc, lockfile hashes, corpus
#                                        SHA-256s, CPU, load average at start and end
#
# The baseline is the tree as checked out. The variant is the same tree with variant.patch
# applied to crates/gamut-png/src/reduce.rs for the duration of one build, and reverted on any
# exit. The script refuses to run if that file already carries uncommitted changes, so it never
# measures, or reverts over, anything but its own patch.
#
# EXPERIMENT_SAMPLES (default 3) sets the timed encodes per input; each input also gets one
# untimed warm-up encode. Byte counts are deterministic, so only encode_ns varies by sample.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/514
raw=$exp/raw
work=target/experiments/514
patch=$exp/variant.patch
site=crates/gamut-png/src/reduce.rs
samples=${EXPERIMENT_SAMPLES:-3}

git diff --quiet HEAD -- "$site" || {
  echo "refusing: $site has uncommitted changes; commit or set them aside first" >&2
  exit 1
}
git apply --check "$patch"

mkdir -p "$work/corpus"
rm -f "$work"/corpus/*.raw "$work"/*.csv
load_start=$(cut -d' ' -f1-3 /proc/loadavg)
head_start=$(git rev-parse HEAD)
dirty_start=$(git diff --quiet HEAD && echo false || echo true)

# Each run writes under target/ first: a failed build, a failed encode or a moved HEAD then
# leaves the committed raw/*.csv untouched, and raw/ changes only alongside run.json.
measure() {
  cargo run --release --quiet -p gamut-png --example experiment_514 -- \
    --variant "$1" --samples "$samples" --corpus-dir "$work/corpus" >"$work/$1.csv"
}

measure baseline
git apply "$patch"
trap 'git apply -R "$patch"' EXIT
measure variant
git apply -R "$patch"
trap - EXIT

load_end=$(cut -d' ' -f1-3 /proc/loadavg)
# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work/baseline.csv" "$work/variant.csv" "$raw/"

sha() { sha256sum "$1" | cut -d' ' -f1; }
json_str() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
corpus_json=$(
  first=1
  for f in "$work"/corpus/*.raw; do
    [ $first -eq 1 ] || printf ','
    first=0
    printf '\n    %s: "%s"' "$(json_str "$(basename "$f")")" "$(sha "$f")"
  done
)
mise_lock=null
[ -f mise.lock ] && mise_lock="\"$(sha mise.lock)\""

cat >"$exp/run.json" <<EOF
{
  "issue": 514,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "variant_patch_sha256": "$(sha "$patch")",
  "rustc": $(json_str "$(rustc -vV | tr '\n' ';')"),
  "mise_lock_sha256": $mise_lock,
  "cargo_lock_sha256": "$(sha Cargo.lock)",
  "rust_toolchain_toml_sha256": "$(sha rust-toolchain.toml)",
  "samples_per_input": $samples,
  "warmup_encodes_per_input": 1,
  "seeds": "none: every input is a deterministic generator in crates/gamut-png/tests/common/corpus.rs or examples/experiment_514.rs",
  "corpus_sha256": {$corpus_json
  },
  "cpu_model": $(json_str "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"),
  "cpu_cores": $(nproc),
  "load_average_start": "$load_start",
  "load_average_end": "$load_end"
}
EOF

# Per-input summary on stdout: bytes for each variant, the race size, and median encode time.
awk -F, '
  FNR == 1 { next }
  {
    key = $2; v = $7
    bytes[key, v] = $9; crc[key, v] = $10; cand[key, v] = $13
    n[key, v]++; t[key, v, n[key, v]] = $11
    if (!(key in seen)) { seen[key] = 1; order[++rows] = key }
  }
  function median(k, v,   m, i, j, a, tmp) {
    m = n[k, v]
    for (i = 1; i <= m; i++) a[i] = t[k, v, i]
    for (i = 1; i <= m; i++) for (j = i + 1; j <= m; j++) if (a[j] < a[i]) { tmp = a[i]; a[i] = a[j]; a[j] = tmp }
    return a[int((m + 1) / 2)]
  }
  END {
    printf "%-24s %9s %9s %9s %6s %6s %12s %12s\n", "input", "base_B", "var_B", "same_file", "base_n", "var_n", "base_ms", "var_ms"
    for (r = 1; r <= rows; r++) {
      k = order[r]
      printf "%-24s %9d %9d %9s %6s %6s %12.1f %12.1f\n", k, bytes[k, "baseline"], bytes[k, "variant"],
        (crc[k, "baseline"] == crc[k, "variant"] && bytes[k, "baseline"] == bytes[k, "variant"]) ? "yes" : "NO",
        cand[k, "baseline"], cand[k, "variant"], median(k, "baseline") / 1e6, median(k, "variant") / 1e6
    }
  }
' "$raw/baseline.csv" "$raw/variant.csv"
