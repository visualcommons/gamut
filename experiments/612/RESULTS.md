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
finished file. Both regress one gated row, `sprite_rgba8 +clean`, by 14 bytes (+1.27%, 1 104 →
1 118). Every other row is byte-identical, finished file for finished file. (#612's own run
measured 18 bytes, 1 148 → 1 166. The data here was regenerated for #887 at a later head, after
#480 changed how the encoder selects filters, which moved four finished files. No verdict moved.)

**Verdict for the caller's palette, #612: no opt-in in that change.** Reordering won on 5 of the
13 palette inputs, by up to 15.9%, but lost on 3, by up to 4.5%. The opt-in would also have to
ship the Zeng ordering on its own, since the derived path keeps luma. That follow-up was filed as
#887.

**Verdict for the caller's palette, #887: shipped as an opt-in, counting raster-consecutive
pixels.** `PngEncoder::with_palette_order(PaletteOrder::ModifiedZeng)` reorders a caller's
palette, and it is off by default. #887 asked for vertical adjacency to be measured before
choosing, because the caller path knows the image width. Vertical adjacency measured worse. Over
the 13 palette inputs, raster order takes the first-seen palettes **6.8%** smaller in total. That
compares with 6.5% for horizontal neighbours within rows alone and 6.0% for horizontal plus
vertical. See [Follow-up #887](#follow-up-887-which-neighbours-the-opt-in-counts).

## What the variants are

- `mzeng.patch`: `build_indexed` orders by modified-Zeng. Neighbours are consecutive pixels in
  raster order, because `analyze8_for` has no image width. Seed with the heaviest co-occurring pair,
  then repeatedly take the unplaced entry with the largest unweighted adjacency to the whole list.
  Attach it to whichever end its adjacency, weighted by `log2(1 + 1/d)` where `d` is the 1-based
  distance from that end, is larger.
- `mzeng-trns.patch`: the same, then a stable partition that moves every non-opaque entry to the
  front in Zeng order. This keeps rule 1 of `ordered_palette`, so the trailing-opaque `tRNS` trim
  still cuts as far as it can.
- `caller-2d.patch` and `caller-rows.patch` (#887) leave the derived path alone. They change the
  neighbours the shipped caller-palette opt-in (`PngPalette::modified_zeng`, the `mzeng-trns`
  algorithm with fixed-point distance weights) counts, and they hand it the image width to do so.
  The baseline counts consecutive pixels in raster order, as `mzeng` does. `caller-2d` counts
  horizontal neighbours within each row plus vertical neighbours. `caller-rows` counts the
  horizontal neighbours alone, without the pair that raster order forms across each row
  boundary.

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
`crates/gamut-png/examples/experiment_612.rs` five times: at the recorded head (`baseline`), then
with each patch applied. The committed data was recorded with `EXPERIMENT_SAMPLES=1`, so one timed
encode follows the warm-up. Byte counts do not depend on the sample count. The `size_contract` group is that test's budget table, row for row, at its
sides and settings (`Level::Best`, `FilterStrategy::BruteForce`, auto-reduce, cleanup per row). The
`reach` group covers palette-reaching content the corpus lacks: photo content posterised to 3-3-2
and 2-2-2 bits, a 3-3-2 posterised XOR gradient, a 16-level grey ramp, and a posterised sprite with
a translucent ring. Each is measured at 128 and 256.

### Finished files (bytes)

| input | colour type | luma (shipped) | mzeng | mzeng-trns |
| --- | :---: | ---: | ---: | ---: |
| sprite_rgba8 +clean | 3 | **1 104** | 1 118 | 1 118 |
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
| photo222_rgb8@256 | 9 922 | 12 135 | 12 138 | 12 138 | **12 134** | +0.02% | +0.03% |
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
mutation surface. It is real work, evidenced only on synthetic inputs, so #612 filed it as #887
rather than folding it into this experiment.

## Follow-up #887: which neighbours the opt-in counts

#887 shipped the opt-in (`PaletteOrder::ModifiedZeng`). The arm's `reordered_bytes` column is the
first-seen palette above, encoded through `encode_indexed8` with the opt-in on. There is no race,
so this is the file the caller gets. The three adjacency rules are the baseline build (raster
order, shipped) and the `caller-2d` and `caller-rows` builds. Bytes, and the change against
first seen:

| input | first seen | raster (shipped) | 2-D | rows |
| --- | ---: | ---: | ---: | ---: |
| flat_rgba8 | 96 | 96 (0.0%) | 96 (0.0%) | 96 (0.0%) |
| palette64_rgba8 | 450 | **425** (−5.6%) | 437 (−2.9%) | **425** (−5.6%) |
| opaque256_rgba8 | 945 | **945** (0.0%) | 960 (+1.6%) | **945** (0.0%) |
| demotable_rgb16 | 317 | **317** (0.0%) | 335 (+5.7%) | **317** (0.0%) |
| photo332_rgb8@128 | 7 953 | 6 686 (−15.9%) | 6 796 (−14.5%) | **6 670** (−16.1%) |
| photo222_rgb8@128 | 3 913 | **3 710** (−5.2%) | 3 769 (−3.7%) | 3 718 (−5.0%) |
| gradient332_rgb8@128 | 1 863 | 1 900 (+2.0%) | 1 879 (+0.9%) | **1 856** (−0.4%) |
| levels16_gray8@128 | 456 | 456 (0.0%) | 456 (0.0%) | 456 (0.0%) |
| photo_sprite_rgba8@128 | 4 330 | **3 923** (−9.4%) | 3 981 (−8.1%) | **3 923** (−9.4%) |
| photo332_rgb8@256 | 21 475 | **19 095** (−11.1%) | 19 366 (−9.8%) | 19 190 (−10.6%) |
| photo222_rgb8@256 | 12 134 | **12 138** (+0.03%) | 12 143 (+0.07%) | **12 138** (+0.03%) |
| gradient332_rgb8@256 | 4 917 | 5 140 (+4.5%) | **5 034** (+2.4%) | 5 233 (+6.4%) |
| levels16_gray8@256 | 639 | 639 (0.0%) | 639 (0.0%) | 639 (0.0%) |
| **total** | 59 488 | **55 470** (−6.75%) | 55 891 (−6.05%) | 55 606 (−6.52%) |

**Raster order is shipped.** It has the smallest total, and its worst row (+4.5%,
`gradient332@256`) is smaller than the worst row of either alternative: +5.7% for 2-D
(`demotable_rgb16`) and +6.4% for rows alone (`gradient332@256`). Counting vertical neighbours
costs bytes. The 2-D rule loses to raster on 8 of the 13 inputs, ties on 3, and wins only on the
two gradients. A plausible reading is that DEFLATE matches along the byte stream, so the
neighbour that pays is the next one in the stream. This experiment does not test that reading.
Raster and rows-only differ only by the one pair per row boundary that raster order adds. That
pair moves five rows, in both directions, and the total by 0.23%. The corpus cannot tell a
mechanism from chance at that size, so the smaller total decides.

**The opt-in is a net win, not a uniform one.** Against the caller's own first-seen order, raster
order wins on 5 inputs (−5.2% to −15.9%), ties on 5, and loses on 3. Two of those losses are the
3-3-2 posterised XOR gradient (+2.0%, +4.5%), and the third is `photo222@256` by 4 bytes. That is
why it is opt-in and documented as a heuristic to measure on your own material, rather than a
default.

## Limits

- The corpus is synthetic. It is the crate's own generators plus the posterised reach inputs, and
  there is no vendored image corpus (`crates/gamut-png/README.md`). Real palette art, such as pixel
  art, icons or dithered GIF conversions, is where an indexed candidate usually wins the race, and
  this corpus has little of it. The candidate-level column is the evidence that would carry over.
- The derived-path variants count raster-consecutive pixels only, because `analyze8_for` never
  learns the image width. The caller path does know it, and #887 measured vertical neighbours
  there (above). They lost. That is some evidence that the derived path is not missing much by
  being unable to count them.
- #887's issue asked for the opt-in's win to be confirmed on real palette images, ideally (pixel
  art, icons, GIF conversions). That confirmation is not here. The repository vendors no such
  corpus, and the 3-3-2 gradient rows already show the reorder can lose. The opt-in's docs
  therefore tell callers to measure it on their own material.
- Wall-clock times are in `raw/` but carry no weight. The committed run took one timed sample after
  one warm-up, and other builds shared the machine (the load average is in `run.json`). The
  verdicts rest on byte counts, which are deterministic.

## Files

- `run.sh` is the whole procedure. It refuses to run over uncommitted changes to the files the
  patches touch (`reduce.rs`, `palette.rs`, `encoder.rs`), and it refuses to record if HEAD moved
  during the run. It prints both summary tables.
- `mzeng.patch`, `mzeng-trns.patch`, `caller-2d.patch` and `caller-rows.patch` are the variants,
  generated with `git diff`.
- `raw/{baseline,mzeng,mzeng-trns,caller-2d,caller-rows}.csv` have one row per input × variant ×
  sample (22 inputs × 1 sample). The columns are documented in the measurement arm's module doc.
- `run.json` records provenance: the git SHA that was built (clean tree), rustc, the `Cargo.lock`,
  `rust-toolchain.toml` and patch hashes (the repository has no `mise.lock`, recorded as `null`),
  the SHA-256 of every input's raw samples, the CPU, and the load average at start and end. The
  inputs are deterministic generators, so there are no seeds.
