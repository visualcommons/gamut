# Experiment #514: race an equal-raw-size chunk-free candidate

**Claim under test.** In `gamut-png`'s `reduce::analyze8_for`, naming a chunk-free runner-up when
`free_size <= input_size`, instead of the shipped strict `free_size < input_size`, would let
`write_reduced_or_native` find a smaller file somewhere: a candidate whose *raw* size merely equals
the input's might still compress differently.

**Acceptance rule** (maintainer decision on #514). Merge only if no `tests/size_contract.rs` row
regresses and at least one improves, counting the per-candidate encode cost. Otherwise record a
negative result in `crates/gamut-png/STATUS.md`.

**Verdict: negative. Kept as `<`.** All 18 inputs produce byte-identical files under both rules
(same length and CRC-32). No row regresses and none improves. The six inputs where the variant
changes anything pay one extra whole encode for it.

## Why the result could not have been otherwise

The chunk-free sizes are grey `⌊n·d/8⌋` (`d ∈ {1,2,4,8}`), grey+alpha `2n` and alpha-drop `3n`
(gated on `c = 4`), compared against an input of `n·c` bytes:

- grey equals the input only when `d = 8c`, which means `c = 1, d = 8`: a `Gray8` input;
- grey+alpha equals it only when `c = 2`: a `GrayAlpha8` input;
- the alpha drop is `3n < 4n` and never ties.

So a tie is always the input re-spelled: the same colour type, the same depth and the same samples
(`Gray { depth: 8 }` carries `px[0] / gray8_scale(8) = px[0]`, and `GrayAlpha8` carries
`[px[0], px[1]]`). Its encode is the twin of the unreduced candidate, which the race already
contains. The issue's argument is that two equal-size candidates can compress differently "once
the colour type changes", and at this site the colour type cannot change.

The 16-bit path changes neither its output nor its cost. A demotable `Gray16`/`GrayAlpha16` input is
re-analysed at 8 bits, and where that returns no runner-up `analyze16_for` already substitutes the
plain demotion, which is the very candidate `<=` would have named.

## Data

`mise run experiment 514` regenerates `raw/` and `run.json` from scratch. It builds
`crates/gamut-png/examples/experiment_514.rs` once at the recorded head (`baseline`), and once
more with `variant.patch` applied (`variant`, the single `<` → `<=` edit). The `size_contract`
group is that test's budget table, row for row, at its sides and settings (`Level::Best`,
`FilterStrategy::BruteForce`, auto-reduce, cleanup per row). The `reach` group holds 8-bit grey and
grey+alpha inputs on which a palette or a colour key wins the raw estimate. Those are the only
layouts where a tie can occur, and no `size_contract` row is one of them.

| input | bytes (both) | same file | race candidates base → variant | median encode ms base → variant |
| --- | ---: | :---: | :---: | ---: |
| gradient_rgb8 | 577 | yes | 1 → 1 | 2706.7 → 2491.6 |
| photo_rgb8 | 5594 | yes | 1 → 1 | 3364.8 → 2766.8 |
| noise_rgb8 | 49348 | yes | 1 → 1 | 75.8 → 60.8 |
| grey_as_rgb8 | 146 | yes | 1 → 1 | 281.3 → 257.5 |
| flat_rgba8 | 96 | yes | 3 → 3 | 2742.0 → 2547.2 |
| sprite_rgba8 | 1651 | yes | 1 → 1 | 3860.1 → 3741.8 |
| sprite_rgba8 +clean | 1148 | yes | n/a | 7688.6 → 7768.8 |
| palette64_rgba8 | 364 | yes | 2 → 2 | 2692.1 → 2232.6 |
| palette64_rgba8 +clean | 364 | yes | n/a | 4499.7 → 5706.7 |
| opaque256_rgba8 | 317 | yes | 3 → 3 | 3450.7 → 4394.1 |
| demotable_rgb16 | 171 | yes | 3 → 3 | 876.4 → 1043.4 |
| tiny_rgb8 | 119 | yes | 1 → 1 | 4.9 → 5.1 |
| levels16_gray8@128 | 363 | yes | **2 → 3** | 861.4 → 2195.8 |
| levels16_ga8@128 | 526 | yes | **2 → 3** | 4373.9 → 7086.7 |
| keyed_ga8@128 | 709 | yes | **2 → 3** | 2770.5 → 5824.9 |
| levels16_gray8@256 | 639 | yes | **2 → 3** | 4678.4 → 11970.9 |
| levels16_ga8@256 | 988 | yes | **2 → 3** | 14178.5 → 24148.7 |
| keyed_ga8@256 | 1363 | yes | **2 → 3** | 11943.6 → 21475.9 |

**How to read the cost column.** `race candidates` counts the whole filter-plus-DEFLATE encodes
`write_reduced_or_native` performs, which is what the issue's cost criterion measures. It is
deterministic, so it is the column the verdict rests on. Each `BruteForce` candidate is itself
seven passes, so 2 → 3 adds seven passes per file. The wall-clock medians (3 timed samples after
one warm-up, per input and variant) are supporting evidence only. `run.json` records a load
average of 17.8–26.9 on 16 threads, because other builds shared the machine. The variance shows
in the unaffected rows, which move by as much as 27% in either direction while doing identical work. The
reach rows still slow down every time, by 1.6× to 2.6×. `+clean` rows leave `race candidates`
empty: their analysis runs on cleaned samples, which the measurement arm cannot see, and the
variant cannot reach them anyway (RGBA input).

## Files

- `run.sh` is the whole procedure. It refuses to run over uncommitted changes to `reduce.rs`, and
  refuses to record if HEAD moved during the run.
- `variant.patch` is the variant: one operator.
- `raw/baseline.csv` and `raw/variant.csv` have one row per input × variant × sample. The columns
  are documented in the measurement arm's module doc.
- `run.json` records provenance: the git SHA that was built, rustc, the `Cargo.lock` and
  `rust-toolchain.toml` hashes (the repository has no `mise.lock`, recorded as `null`), the
  SHA-256 of every input's raw samples, the CPU, and the load average at start and end. The
  inputs are deterministic generators, so there are no seeds.
