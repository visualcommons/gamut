# Experiment #612: modified-Zeng palette ordering

**Claim under test.** Ordering `gamut-png`'s encoder-derived palette by modified-Zeng (Zeng, Li
and Lei, ICIP 2000, as modified by Pinho and Neves, IEEE TIP 13(11), 2004), instead of the shipped
two-key sort in `reduce::ordered_palette` (transparent entries first, then Rec. 601 luma), makes
finished files smaller. Second question, decided after the first: whether a **caller-supplied**
palette (`encode_indexed8`) should be reordered, behind an explicit opt-in (maintainer decision on
#612).

**Acceptance rule.** The default flips only if the measurements support it: no
`tests/size_contract.rs` row may regress, and some row must improve.

**Verdict for the default: negative. The luma order stays.** Neither variant improves a single
finished file. Both regress one gated row, `sprite_rgba8 +clean`, by 18 bytes (+1.57%, 1 148 →
1 166). Every other row is byte-identical, finished file for finished file.

**Verdict for the caller's palette: no opt-in in this change.** Reordering wins on 5 of the 13
palette inputs here, by up to 15.9%, but it loses on 3, by up to 4.5%. The opt-in would also have to
ship the Zeng ordering on its own, since the derived path keeps luma. That follow-up is filed
separately with these numbers.

## What the variants are

- `mzeng.patch`: `build_indexed` orders by modified-Zeng. Neighbours are consecutive pixels in
  raster order, because `analyze8_for` has no image width. Seed with the heaviest co-occurring pair,
  then repeatedly take the unplaced entry with the largest unweighted adjacency to the whole list.
  Attach it to whichever end its adjacency, weighted by `log2(1 + 1/d)` where `d` is the 1-based
  distance from that end, is larger.
- `mzeng-trns.patch`: the same, then a stable partition that moves every non-opaque entry to the
  front in Zeng order. This keeps rule 1 of `ordered_palette`, so the trailing-opaque `tRNS` trim
  still cuts as far as it can.

## Why the finished files barely move

`write_reduced_or_native` races the indexed candidate against the unreduced encode, and against a
chunk-free runner-up where one exists. It keeps the smaller file. An ordering changes only the
indexed candidate, so it reaches a finished file only on an input where that candidate wins the
race. In this corpus that is three inputs: `flat_rgba8`, `levels16_gray8@256` and
`sprite_rgba8 +clean`. The first two have one and sixteen entries, and every order of them
compresses the same. On the third, Zeng loses.

So the measurement arm also records the indexed candidate **on its own** (`indexed_bytes`), whatever
wins the race. That is the ordering's own signal. Modified-Zeng with the transparent partition
beats luma on 8 of 13 inputs, by 1.9% to 16.5%. It ties on 3 and loses on 2:
`gradient332_rgb8@128` by 11.2%, and `photo222_rgb8@256` by 3 bytes. None of those wins flips a race. The posterised photo content goes from 6 815 to
6 686 bytes as an indexed file, and the RGB file it loses to is 3 926 bytes.

## Data

`mise run experiment 612` regenerates `raw/` and `run.json` from scratch. It builds
`crates/gamut-png/examples/experiment_612.rs` three times: at the recorded head (`baseline`), then
with each patch applied. The `size_contract` group is that test's budget table, row for row, at its
sides and settings (`Level::Best`, `FilterStrategy::BruteForce`, auto-reduce, cleanup per row). The
`reach` group covers palette-reaching content the corpus lacks: photo content posterised to 3-3-2
and 2-2-2 bits, a 3-3-2 posterised XOR gradient, a 16-level grey ramp, and a posterised sprite with
a translucent ring. Each is measured at 128 and 256.

### Finished files (bytes)

| input | colour type | luma (shipped) | mzeng | mzeng-trns |
| --- | :---: | ---: | ---: | ---: |
| sprite_rgba8 +clean | 3 | **1 148** | 1 166 | 1 166 |
| every other row (21) | — | identical | identical | identical |

### The indexed candidate alone (bytes, `indexed_bytes` / `first_seen_bytes`)

Rows where the analysis offers a palette. `first seen` is the same palette renumbered by first
appearance in raster order. It is what a caller building its palette while scanning would hand
`encode_indexed8`, and that path keeps the order. The column is identical across builds.

| input | finished file | luma | mzeng | mzeng-trns | first seen | mzeng-trns vs luma | mzeng-trns vs first seen |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| flat_rgba8 | 96 | 96 | 96 | 96 | 96 | 0.0% | 0.0% |
| palette64_rgba8 | 364 | 475 | 450 | **425** | 450 | −10.5% | −5.6% |
| opaque256_rgba8 | 317 | 1 132 | **945** | **945** | **945** | −16.5% | 0.0% |
| demotable_rgb16 | 171 | 367 | **317** | **317** | **317** | −13.6% | 0.0% |
| photo332_rgb8@128 | 3 926 | 6 815 | **6 686** | **6 686** | 7 953 | −1.9% | −15.9% |
| photo222_rgb8@128 | 3 111 | 3 837 | **3 710** | **3 710** | 3 913 | −3.3% | −5.2% |
| gradient332_rgb8@128 | 757 | **1 708** | 1 900 | 1 900 | 1 863 | +11.2% | +2.0% |
| levels16_gray8@128 | 363 | 456 | 456 | 456 | 456 | 0.0% | 0.0% |
| photo_sprite_rgba8@128 | 2 927 | 4 035 | 4 019 | **3 923** | 4 330 | −2.8% | −9.4% |
| photo332_rgb8@256 | 13 038 | 19 818 | **19 095** | **19 095** | 21 475 | −3.6% | −11.1% |
| photo222_rgb8@256 | 9 970 | 12 135 | 12 138 | 12 138 | **12 134** | +0.02% | +0.03% |
| gradient332_rgb8@256 | 2 020 | 5 712 | 5 140 | 5 140 | **4 917** | −10.0% | +4.5% |
| levels16_gray8@256 | 639 | 639 | 639 | 639 | 639 | 0.0% | 0.0% |

`indexed_bytes` is empty on the rows where the analysis offers no palette: a chunk-free reduction
or a colour key wins the estimate, or the input has more than 256 colours. It is also empty on the
`+clean` rows, whose analysis runs on cleaned samples the arm cannot see. Those rows are omitted
above.

### Reading the caller-palette column

A caller's palette is emitted as given: there is no race. For that path the indexed sizes **are**
the finished sizes, so `first seen → mzeng-trns` is what an opt-in reorder would buy. It wins on 5
inputs, by 5.2% to 15.9%, and ties on 5. It loses on 3: the two gradient rows, by 2.0% and 4.5%,
and `photo222_rgb8@256`, by 4 bytes. Luma,
the order the derived path keeps, would be a worse opt-in: it loses to first-seen by 15.8% to 19.8%
on `opaque256`/`demotable` and by 16.2% on `gradient332@256`. So the opt-in would need Zeng shipped
for it alone. That is a new public encoder option and a second ordering function with its own
mutation surface. It is real work, evidenced only on synthetic inputs, and it is filed rather than
folded into this experiment.

## Limits

- The corpus is synthetic. It is the crate's own generators plus the posterised reach inputs, and
  there is no vendored image corpus (`crates/gamut-png/README.md`). Real palette art, such as pixel
  art, icons or dithered GIF conversions, is where an indexed candidate usually wins the race, and
  this corpus has little of it. The candidate-level column is the evidence that would carry over.
- Adjacency is raster-consecutive only. `analyze8_for` never learns the image width, so vertical
  neighbours, which `Up`/`Paeth` predict from, are invisible to this Zeng. Threading the width in
  would change a crate-internal signature, and this experiment measured the ordering the existing
  seam can reach.
- Wall-clock medians (3 timed samples after one warm-up) are in `raw/` but carry no weight.
  `run.json` records a load average of 27.7–40.2 on 16 threads, because other builds shared the
  machine. The verdict rests on byte counts, which are deterministic.

## Files

- `run.sh` is the whole procedure. It refuses to run over uncommitted changes to `reduce.rs`, and it
  refuses to record if HEAD moved during the run.
- `mzeng.patch` and `mzeng-trns.patch` are the variants, generated with `git diff`.
- `raw/{baseline,mzeng,mzeng-trns}.csv` have one row per input × variant × sample (22 inputs × 3
  samples). The columns are documented in the measurement arm's module doc.
- `run.json` records provenance: the git SHA that was built (clean tree), rustc, the `Cargo.lock`,
  `rust-toolchain.toml` and patch hashes (the repository has no `mise.lock`, recorded as `null`),
  the SHA-256 of every input's raw samples, the CPU, and the load average at start and end. The
  inputs are deterministic generators, so there are no seeds.
