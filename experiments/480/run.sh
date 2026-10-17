#!/usr/bin/env bash
# Experiment #480: the filter-selection techniques oxipng ships that gamut-png did not -- BigEnt,
# two-tier trials, AtomicMin pruning and per-line trial deflate -- against the shipped heuristics.
#
# Regenerates everything under experiments/480/ except RESULTS.md (which reads it):
#   raw/measurements.csv -- one row per input x variant x sample, from
#                           crates/gamut-png/examples/experiment_480.rs (columns in its module doc)
#   run.json             -- provenance: git SHA, rustc, lockfile hashes, corpus SHA-256s, CPU,
#                           load average at start and end
#
# One build of the tree as checked out: every variant is assembled from the crate's public stages,
# so nothing is patched. The arm panics, and nothing is recorded, if its reconstruction of the
# shipped MinSumAbs, MinTrialDeflate or BruteForce disagrees with the crate.
#
# EXPERIMENT_SAMPLES (default 3) sets the timed passes per input; each input also gets one untimed
# warm-up encode. Byte counts are deterministic, so only time_ns varies by sample.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
exp=experiments/480
raw=$exp/raw
work=target/experiments/480
samples=${EXPERIMENT_SAMPLES:-3}

mkdir -p "$work/corpus"
rm -f "$work"/corpus/*.raw "$work"/measurements.csv
load_start=$(cut -d' ' -f1-3 /proc/loadavg)
head_start=$(git rev-parse HEAD)
dirty_start=$(git diff --quiet HEAD && echo false || echo true)

# The run writes under target/ first: a failed build, a failed cross-check or a moved HEAD then
# leaves the committed raw/measurements.csv untouched, and raw/ changes only alongside run.json.
cargo run --release --quiet -p gamut-png --example experiment_480 -- \
  --samples "$samples" --corpus-dir "$work/corpus" >"$work/measurements.csv"

load_end=$(cut -d' ' -f1-3 /proc/loadavg)
# The SHA names what was built, so a commit landing mid-run would make it name something else.
[ "$(git rev-parse HEAD)" = "$head_start" ] || {
  echo "refusing to record: HEAD moved from $head_start during the run; re-run" >&2
  exit 1
}
mkdir -p "$raw"
cp "$work/measurements.csv" "$raw/"

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
  "issue": 480,
  "git_sha": "$head_start",
  "git_dirty": $dirty_start,
  "rustc": $(json_str "$(rustc -vV | tr '\n' ';')"),
  "mise_lock_sha256": $mise_lock,
  "cargo_lock_sha256": "$(sha Cargo.lock)",
  "rust_toolchain_toml_sha256": "$(sha rust-toolchain.toml)",
  "samples_per_input": $samples,
  "warmup_encodes_per_input": 1,
  "seeds": "none: every input is a deterministic generator in crates/gamut-png/tests/common/corpus.rs",
  "corpus_sha256": {$corpus_json
  },
  "cpu_model": $(json_str "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"),
  "cpu_cores": $(nproc),
  "load_average_start": "$load_start",
  "load_average_end": "$load_end"
}
EOF

# Summary on stdout: IDAT bytes per input for each variant (identical across samples), and the
# median time_ns in milliseconds.
awk -F, '
  FNR == 1 { next }
  {
    key = $1; v = $5
    bytes[key, v] = $7
    n[key, v]++; t[key, v, n[key, v]] = $9
    if (!(key in seen)) { seen[key] = 1; order[++rows] = key }
    if (!(v in vseen)) { vseen[v] = 1; vorder[++vars] = v }
  }
  function median(k, v,   m, i, j, a, tmp) {
    m = n[k, v]
    for (i = 1; i <= m; i++) a[i] = t[k, v, i]
    for (i = 1; i <= m; i++) for (j = i + 1; j <= m; j++) if (a[j] < a[i]) { tmp = a[i]; a[i] = a[j]; a[j] = tmp }
    return a[int((m + 1) / 2)]
  }
  END {
    for (x = 1; x <= vars; x++) {
      v = vorder[x]
      printf "%-22s", v
      for (r = 1; r <= rows; r++) printf " %s=%d/%.0fms", order[r], bytes[order[r], v], median(order[r], v) / 1e6
      printf "\n"
    }
  }
' "$raw/measurements.csv"
