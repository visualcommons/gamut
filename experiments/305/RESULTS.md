# Experiment #305: gamut-jpeg decode throughput against zune-jpeg

**Claim under test.** #305 measured `gamut-jpeg`'s decoder at 7-9x zune-jpeg's decode time on real camera JPEGs (24-45 MP). It named the Huffman/bit-reader hot path, the IDCT, and per-MCU allocation as the likely causes. This experiment measures the gap in-repo, profiles where the time goes, and measures what the decoder work bought.

**Acceptance rule** (maintainer decision on #305):

- Timings are reported as ratios against zune-jpeg in the same session, at 100 or more samples.
- Decoded output is still checked against libjpeg-turbo.
- If the gap does not close in one pass, the measured residual is filed.

**This record has three runs.**

1. The first ran on #895 (the entropy and presentation work that closed #305). Its data is in git at the commit that merged #895 (`1575b0db`).
2. The second re-ran the same script after #896 replaced the decoder's `f64` reference IDCT with `gamut-dsp`'s fixed-point `idct8x8_int`. Its data is in git at the commit that merged #898 (`7da7b2f4`).
3. The third, recorded in `raw/` and `run.json` now, re-ran it after #897's entropy-decoder work: a fused run/size/magnitude Huffman probe and a four-byte bit-reader refill.

The earlier runs' numbers are quoted below where they are compared.

**Verdict: the entropy work narrowed the gap without changing an output byte; the remaining gap is spread over all three stages.**

- **Gap to zune-jpeg.** Paired per sample, gamut now takes 2.6-3.1x zune-jpeg's time on baseline streams and 2.0x on the progressive one. After #896 it was 3.1-3.7x and 2.1x. After #895 it was 7.2-9.5x and 3.8x. Before #305 it was 8.7-11.3x and 4.1x. The second run was measured under about three times this run's load, so part of that step is the machine. The output-preserving claim does not depend on it.
- **Against scalar libjpeg-turbo.** gamut now takes 1.2-1.5x the time of the oracle build (`-DWITH_SIMD=0`).
- **Output.** Every `head` fingerprint equals the second run's, on all four streams. The entropy work changed no decoded sample.
- **Residual.** Of gamut's own decode time, the entropy decoder is now 55% (it was 62%), the IDCT 24% and presentation 21%. No single stage dominates any more. What is left is filed below.

## Method

- **Corpus.** `crates/gamut-jpeg/benches/common/corpus.rs` generates deterministic photo-like RGB at 6000x4000 (24 MP, the small end of the issue's range):
  - a smooth separable illumination field, standing in for most of a photograph's area;
  - per-pixel noise from a fixed-seed LCG, ±8 codes per channel, which keeps AC coefficients alive at q90 the way sensor noise does;
  - hard-edged flat rectangles on a coarse grid.

  libjpeg-turbo (the crate's oracle, `tooling/libjpeg-oracle`) encodes it at q90 with its standard tables into four streams: baseline 4:2:0, 4:2:2 and 4:4:4, and progressive 4:2:0 (its standard ten-scan script). Neither decoder under test wrote the streams. The streams' SHA-256s are in `run.json`. They are the same in both runs.
- **Decoders.**
  - `gamut`: `JpegDecoder::decode_image` to `Rgb8`.
  - `zune-jpeg 0.5.15`: default features, x86 SIMD on, to RGB.
  - `libjpeg-turbo 3.2.0`: the vendored oracle build, to RGB. It is built `-DWITH_SIMD=0` (`tooling/libjpeg-oracle/build.rs`), so it is a scalar-C yardstick, not libjpeg-turbo's shipped SIMD speed.

  Each decode is one wall-clock call, including upsampling and colour conversion. Each timing includes the copies its decoder makes on the way out:
  - gamut's presentation copies once through `gamut_core::convert`, which every caller pays;
  - the oracle binding copies once out of libjpeg-turbo's C allocation;
  - zune-jpeg returns the buffer it decoded into.
- **Variants.** `before` is `crates/gamut-jpeg/src` at the commit before #305's work (`before_sha` in `run.json`), swapped in for one build. `head` is the tree as committed (`git_sha` in `run.json`). Everything else is identical between the two builds: the arm, the corpus, the other decoders, and `gamut-dsp`. `before` calls the `f64` `idct8x8`, which is unchanged, so it still measures the pre-#305 decoder. `head` now includes #896's kernel and #897's entropy work.
- **Sampling.** 100 timed decodes per input x decoder x variant. Both binaries are built first, then run in ten alternating rounds (before, head, before, head, ...) of ten samples each, with one untimed warm-up per input x decoder per round. Within a sample the three decoders are timed round-robin. A change in load during the session therefore lands on both variants and all three decoders alike. The machine was shared with other work throughout. `run.json` records the load average at the start and the end: 2.9 at the start and 7.5 at the end, on 16 threads (the second run's were 13.3 and 29.5). That is why the ratios, not the absolute milliseconds, are the result. The absolute times are not comparable between runs.
- **Output check.** Every row carries an FNV-1a fingerprint of the decoded samples and the max/mean absolute difference from libjpeg-turbo's decode.

## Results

**Medians of 100 decodes, 6000x4000, third run.** Lower is better. "before / zune" and "head / zune" are each variant's median over the median of the zune-jpeg decodes timed in the same rounds. "Speed-up" is before over head. "#895 / zune" and "#896 / zune" are the first and second runs' head ratios: the decoder with #895's work and the `f64` IDCT, then with #896's integer IDCT.

| stream | bytes | gamut before | gamut head | zune-jpeg | libjpeg-turbo | before / zune | #895 / zune | #896 / zune | head / zune | speed-up |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline-420` | 4,127,399 | 698 ms | 210 ms | 80 ms | 178 ms | 8.69x | 7.31x | 3.32x | 2.62x | 3.32x |
| `baseline-422` | 4,610,025 | 855 ms | 265 ms | 89 ms | 183 ms | 9.56x | 8.39x | 3.70x | 2.97x | 3.22x |
| `baseline-444` | 6,125,769 | 1153 ms | 337 ms | 110 ms | 228 ms | 10.46x | 9.69x | 3.88x | 3.06x | 3.42x |
| `progressive-420` | 3,610,433 | 823 ms | 394 ms | 200 ms | 263 ms | 4.13x | 3.78x | 2.16x | 1.97x | 2.09x |

(zune-jpeg and libjpeg-turbo columns: the `head` rounds' medians. The `before` rounds' are within 2% of them.)

**The same ratio, paired per sample.** In every sample, gamut and zune-jpeg decoded the same stream back to back in one process. Their ratio sample by sample is therefore the measurement least exposed to the machine's load. Median, with the 10th-90th percentile:

| stream | samples | gamut / zune-jpeg, before | #895 (first run) | #896 (second run) | head | gamut vs libjpeg-turbo, max / mean | zune-jpeg vs libjpeg-turbo, max / mean |
| --- | ---: | --- | --- | --- | --- | --- | --- |
| `baseline-420` | 100 | 8.66x (8.45-9.05) | 7.16x (6.49-8.10) | 3.12x (2.16-4.76) | 2.63x (2.59-2.74) | 7 / 0.0504 | 4 / 0.0416 |
| `baseline-422` | 100 | 9.57x (9.44-10.21) | 8.39x (7.11-10.27) | 3.60x (2.53-4.91) | 2.98x (2.94-3.06) | 6 / 0.0709 | 4 / 0.0482 |
| `baseline-444` | 100 | 10.46x (10.29-10.90) | 9.46x (8.20-10.82) | 3.70x (2.72-5.94) | 3.05x (3.02-3.18) | 0 / 0.0000 | 3 / 0.0094 |
| `progressive-420` | 100 | 4.15x (4.07-4.24) | 3.78x (3.33-4.84) | 2.07x (1.58-3.23) | 1.98x (1.93-2.01) | 7 / 0.0504 | 4 / 0.0416 |

How to read it:

- **The output did not move.** Every `head` fingerprint (`output_fnv64` in `raw/head.csv`) equals the second run's, and so does every difference from libjpeg-turbo. The entropy work changes how bits are read, never what they decode to. The second run's change is still the one that moved the output: #896's kernel is libjpeg-turbo's accurate integer IDCT bit for bit, so 4:4:4 decodes identically to libjpeg-turbo (max 0), and on subsampled streams the 6-7 code max difference left is chroma upsampling (gamut replicates chroma samples; libjpeg-turbo interpolates them).
- **How much of the step is #897's.** The second run was measured under a load average of 13-30, this one under 3-8. The same `before` decoder's paired ratio fell 6.5% between them on `baseline-420` (9.26x to 8.66x), so up to about that much of the 16% step from 3.12x to 2.63x may be the machine, not the code. Within this pull request, an intermediate run at `ebcf473` measured the fused probe alone in a session of similar load (`raw/fused-probe-only/`): 3.01x, 3.37x, 3.51x and 2.23x paired. The refill change took those to the `head` column. That run's profile is the reason for the refill change: with the bit reader topped up on every call, `BitReader::fill` alone was 10.8% of all samples.
- **Progressive moved least.** Its refinement scans (`decode_ac_refine`) still decode one symbol per probe and read correction bits one at a time, and they are most of the progressive stream's entropy work. zune-jpeg's progressive decode costs 2.5x its baseline one (200 against 80 ms); gamut's costs 1.9x (394 against 210 ms).
- **This run was quieter than the second.** The p10-p90 spreads are under a tenth of the second run's. The alternating rounds keep load changes off the comparison between variants, and the ratio to zune-jpeg is taken within each sample. Every claim here is still a median.

## Where the time goes

`raw/profile-head.txt` is a flat `perf` profile of the `head` build: three samples per input x decoder, with all three decoders and the corpus generator in one process. The table shows the symbols that belong to gamut's decode, as a share of all samples and of gamut's own. The earlier runs' shares of gamut's decode are given for comparison:

| part | symbols | of all samples | of gamut's decode | second run | first run |
| --- | --- | ---: | ---: | ---: | ---: |
| IDCT (`gamut-dsp`) | `idct8x8_int` 7.25% | 7.25% | 23.8% | 19.2% | 69.5% |
| entropy decode | `Ctx::decode_block` 5.00%, `decode_internal` 4.35% (the progressive scan loops are inlined there), `BitReader::fill` 3.80%, `decode_coded` 2.69%, `resolve` 0.80% | 16.64% | 54.7% | 62.0% | 23.8% |
| presentation (upsample + YCbCr to RGB + convert) | `DecodeImage<Rgb8>::decode_image` 5.76%, `assemble_progressive` 0.77% | 6.53% | 21.5% | 18.8% | 6.8% |

`decode_symbol` no longer appears as a symbol of its own: the sequential path and the progressive first passes now go through `decode_coded`, and its remaining callers, the refinement scans, are among the loops inlined into `decode_internal`. As a share of all samples, gamut's decode fell from 33.4% to 30.4%. The corpus generator's own symbols are excluded (`experiment_305::main`, and libjpeg-turbo's `forward_DCT`/`encode_*`).

## What is left, and where it is filed

- **Presentation (#905, `gamut-jpeg`).** It is a fifth of gamut's decode and the one stage #897 left alone. Upsampling and YCbCr to RGB are scalar per pixel, where zune-jpeg uses AVX2 (`ycbcr_to_rgb_avx2_1`, `upsample_horizontal_avx2`). Every decode also pays a second full-image copy through `gamut_core::convert`, even when the native layout is the one requested.
- **The IDCT itself.** `idct8x8_int` is scalar. zune-jpeg's AVX2 IDCT (`idct_avx2` 1.43% + `idct_avx2_4x4` 0.63%) costs a little over a quarter of it per decode. SIMD variants behind the kernel's signature are already a `gamut-dsp` follow-up in its `STATUS.md`.
- **The entropy decoder.** It is still the largest stage, but it is now spread over the block loop, the progressive scan loops and the reader, with no single hot symbol. The progressive refinement path is the clearest remaining target. No issue is filed for it: the next profile, after #905, decides whether it leads.

## Scope of #305 and the harness choice

The [delivery record on #305](https://github.com/visualcommons/gamut/issues/305#issuecomment-6236460699) records two departures from the issue's original decision:

- **Scope.** #305 is reduced to the bench, this experiment record and this first pass. The reduction is the maintainer's decision of 2026-10-17, which supersedes the 2026-10-16 note that an unclosed gap lands as `Part of #305`: "Close #305, residual in #896/#897 — #305 becomes 'bench + experiment + first pass', the gap work lives in #896/#897". The residual gap is not part of #305: it is filed as #896 (the `gamut-dsp` IDCT, which this record's second run measures) and #897 (the `gamut-jpeg` follow-up, which the third run measures).
- **Harness.** The issue's decision named criterion with at least 100 samples. The bench (`crates/gamut-jpeg/benches/decode.rs`) uses divan instead, following the workspace's single-harness convention (#149, `docs/benchmarking.md`). It keeps the 100 samples per stream shape (`sample_count = 100`). This experiment's own numbers come from its arm (`examples/experiment_305.rs`), which also takes 100 samples per input x decoder x variant.

## Reproducing

`mise run experiment 305` regenerates `raw/before.csv`, `raw/head.csv`, `raw/profile-head.txt` and `run.json`. It needs the `third_party/libjpeg-turbo` submodule and a C toolchain for the oracle. Its stdout prints the first table's medians and ratios, except the `#895 / zune` and `#896 / zune` columns, which come from the first and second runs' `raw/head.csv` at `1575b0db` and `7da7b2f4`. `raw/fused-probe-only/` holds the intermediate run at `ebcf473` (its own `run.json`, `before.csv`, `head.csv` and `profile-head.txt`); the script does not regenerate it, and it is reproduced by checking out `ebcf473` and running the same command. The paired table is the per-sample gamut / zune-jpeg ratio over the same rows (rows with equal `input` and `sample` in one CSV). `EXPERIMENT_SAMPLES`, `EXPERIMENT_ROUNDS`, `EXPERIMENT_WIDTH` and `EXPERIMENT_HEIGHT` override the defaults (100, 10, 6000, 4000).
