# Experiment #305: gamut-jpeg decode throughput against zune-jpeg

**Claim under test.** #305 measured `gamut-jpeg`'s decoder at 7-9x zune-jpeg's decode time on real camera JPEGs (24-45 MP). It named the Huffman/bit-reader hot path, the IDCT, and per-MCU allocation as the likely causes. This experiment measures the gap in-repo, profiles where the time goes, and measures what the decoder work bought.

**Acceptance rule** (maintainer decision on #305):

- Timings are reported as ratios against zune-jpeg in the same session, at 100 or more samples.
- Decoded output is still checked against libjpeg-turbo.
- If the gap does not close in one pass, the measured residual is filed.

**This record has two runs.** The first ran on #895 (the entropy and presentation work that closed #305). The second, recorded in `raw/` and `run.json` now, re-ran the same script after #896 replaced the decoder's `f64` reference IDCT with `gamut-dsp`'s fixed-point `idct8x8_int`. The first run's data is in git at the commit that merged #895 (`1575b0db`); its numbers are quoted below where they are compared.

**Verdict: the IDCT is no longer the bottleneck; the residual is the entropy decoder.**

- **Speed-up.** Against the pre-#305 decoder, `gamut-jpeg`'s decode is now 2.0-3.1x faster. In the first run, #895 alone made it 1.14-1.26x faster.
- **Gap to zune-jpeg.** The gap is now 3.1-3.7x on baseline streams and 2.1x on the progressive one. After #895 it was 7.2-9.5x and 3.8x. Before #305 it was 8.7-11.3x and 4.1x.
- **Output.** On 4:4:4 gamut's decode is now byte-identical to libjpeg-turbo's (max difference 0; it was 3). On subsampled streams the only difference left is chroma upsampling.
- **Residual.** The IDCT is now 19% of gamut's decode time, down from 70%. The entropy decoder is 62%. It is filed as #897.

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
- **Variants.** `before` is `crates/gamut-jpeg/src` at the commit before #305's work (`before_sha` in `run.json`), swapped in for one build. `head` is the tree as committed (`git_sha` in `run.json`). Everything else is identical between the two builds: the arm, the corpus, the other decoders, and `gamut-dsp`. `before` calls the `f64` `idct8x8`, which is unchanged, so it still measures the pre-#305 decoder. `head` now includes #896's kernel.
- **Sampling.** 100 timed decodes per input x decoder x variant. Both binaries are built first, then run in ten alternating rounds (before, head, before, head, ...) of ten samples each, with one untimed warm-up per input x decoder per round. Within a sample the three decoders are timed round-robin. A change in load during the session therefore lands on both variants and all three decoders alike. The machine was shared with other work throughout. `run.json` records the load average at the start and the end: 13.3 at the start and 29.5 at the end, on 16 threads. That is why the ratios, not the absolute milliseconds, are the result. The absolute times are not comparable between the two runs.
- **Output check.** Every row carries an FNV-1a fingerprint of the decoded samples and the max/mean absolute difference from libjpeg-turbo's decode.

## Results

**Medians of 100 decodes, 6000x4000, second run.** Lower is better. "before / zune" and "head / zune" are each variant's median over the median of the zune-jpeg decodes timed in the same rounds. "Speed-up" is before over head. "#895 / zune" is the first run's head ratio: the decoder with #895's work and the `f64` IDCT.

| stream | bytes | gamut before | gamut head | zune-jpeg | libjpeg-turbo | before / zune | #895 / zune | head / zune | speed-up |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline-420` | 4,127,399 | 1312 ms | 421 ms | 127 ms | 327 ms | 9.71x | 7.31x | 3.32x | 3.12x |
| `baseline-422` | 4,610,025 | 1584 ms | 562 ms | 152 ms | 338 ms | 11.72x | 8.39x | 3.70x | 2.82x |
| `baseline-444` | 6,125,769 | 2187 ms | 735 ms | 190 ms | 413 ms | 11.92x | 9.69x | 3.88x | 2.98x |
| `progressive-420` | 3,610,433 | 1606 ms | 795 ms | 367 ms | 514 ms | 4.30x | 3.78x | 2.16x | 2.02x |

(zune-jpeg and libjpeg-turbo columns: the `head` rounds' medians. The `before` rounds' are within 11% of them.)

**The same ratio, paired per sample.** In every sample, gamut and zune-jpeg decoded the same stream back to back in one process. Their ratio sample by sample is therefore the measurement least exposed to the machine's load. Median, with the 10th-90th percentile:

| stream | samples | gamut / zune-jpeg, before | gamut / zune-jpeg, #895 (first run) | gamut / zune-jpeg, head | gamut vs libjpeg-turbo, max / mean | zune-jpeg vs libjpeg-turbo, max / mean |
| --- | ---: | --- | --- | --- | --- | --- |
| `baseline-420` | 100 | 9.26x (5.56-14.23) | 7.16x (6.49-8.10) | 3.12x (2.16-4.76) | 7 / 0.0504 | 4 / 0.0416 |
| `baseline-422` | 100 | 9.99x (6.81-15.26) | 8.39x (7.11-10.27) | 3.60x (2.53-4.91) | 6 / 0.0709 | 4 / 0.0482 |
| `baseline-444` | 100 | 11.58x (7.49-16.37) | 9.46x (8.20-10.82) | 3.70x (2.72-5.94) | 0 / 0.0000 | 3 / 0.0094 |
| `progressive-420` | 100 | 4.24x (3.26-5.98) | 3.78x (3.33-4.84) | 2.07x (1.58-3.23) | 7 / 0.0504 | 4 / 0.0416 |

How to read it:

- **The output moved, toward libjpeg-turbo.** The new kernel is libjpeg-turbo's accurate integer IDCT bit for bit, so `head`'s fingerprints differ from `before`'s on every stream. On 4:4:4 gamut's decode is now identical to libjpeg-turbo's (max 0, mean 0; it was 3 / 0.0331). On subsampled streams the max difference is unchanged at 6-7 codes, because it comes from chroma upsampling: gamut replicates chroma samples, and libjpeg-turbo interpolates them ("fancy" upsampling). The mean difference halved there (0.0960 to 0.0504 on 4:2:0), because the IDCT's share of it is gone. `crates/gamut-jpeg/tests/oracle.rs` now asserts the gray and 4:4:4 decode parity as exact equality.
- **Progressive is still the closest shape.** zune-jpeg's progressive decode costs 2.9x its baseline one (367 against 127 ms). gamut's costs 1.9x (795 against 421 ms).
- **The run was noisier than the first.** Load rose from 13 to 30 during the session, and the p10-p90 spreads are about twice the first run's. Per round, the before/head speed-up ranged from 0.92x to 9.30x around the medians above. The alternating rounds keep that noise off the comparison between variants. The ratio to zune-jpeg is taken within each sample. Every claim here is still a median.

## Where the time goes

`raw/profile-head.txt` is a flat `perf` profile of the `head` build: three samples per input x decoder, with all three decoders and the corpus generator in one process. The table shows the symbols that belong to gamut's decode, as a share of all samples and of gamut's own. The first run's shares of gamut's decode are given for comparison:

| part | symbols | of all samples | of gamut's decode | first run |
| --- | --- | ---: | ---: | ---: |
| IDCT (`gamut-dsp`) | `idct8x8_int` 6.41% | 6.41% | 19.2% | 69.5% |
| entropy decode | `decode_symbol` 9.48%, `Ctx::decode_block` 6.24%, `decode_internal` 3.90% (the progressive scan loops are inlined there), `BitReader::fill` 1.10% | 20.72% | 62.0% | 23.8% |
| presentation (upsample + YCbCr to RGB + convert) | `DecodeImage<Rgb8>::decode_image` 5.53%, `assemble_progressive` 0.75% | 6.28% | 18.8% | 6.8% |

`__cos_fma` and `round` are gone from the profile: they came only from the `f64` kernel's per-block cosine table. `idct8x8_int` (6.41%) now costs a little over half of libjpeg-turbo's scalar `jpeg_idct_islow` (11.10%), which computes the same values. The corpus generator's own symbols are excluded (`experiment_305::main`, and libjpeg-turbo's `forward_DCT`/`encode_*`).

## What is left, and where it is filed

- **The entropy decoder and presentation (#897, `gamut-jpeg`).** This is now most of the gap. The entropy decoder is 62% of gamut's decode time, and presentation another 19%. #897 covers a fused AC run/size + magnitude fast path, a four-byte bit-reader refill, and presentation's scalar loops and full-image copy. zune-jpeg's `ac_lookup` and four-byte refill are the reference points. Its first item, adopting the new kernel in the decoder, is done in #896: both paths run `idct8x8_int`, and the sequential path passes the DC-only hint.
- **The IDCT itself.** `idct8x8_int` is scalar. zune-jpeg's AVX2 IDCT (`idct_avx2`, 1.06% + 0.48%) is about four times cheaper per decode. SIMD variants behind the kernel's signature are already a `gamut-dsp` follow-up in its `STATUS.md`. At 19% of the decode, they are not the next bottleneck.

## Scope of #305 and the harness choice

The [delivery record on #305](https://github.com/visualcommons/gamut/issues/305#issuecomment-6236460699) records two departures from the issue's original decision:

- **Scope.** #305 is reduced to the bench, this experiment record and this first pass. The reduction is the maintainer's decision of 2026-10-17, which supersedes the 2026-10-16 note that an unclosed gap lands as `Part of #305`: "Close #305, residual in #896/#897 — #305 becomes 'bench + experiment + first pass', the gap work lives in #896/#897". The residual gap is not part of #305: it is filed as #896 (the `gamut-dsp` IDCT, which this record's second run measures) and #897 (the `gamut-jpeg` follow-up).
- **Harness.** The issue's decision named criterion with at least 100 samples. The bench (`crates/gamut-jpeg/benches/decode.rs`) uses divan instead, following the workspace's single-harness convention (#149, `docs/benchmarking.md`). It keeps the 100 samples per stream shape (`sample_count = 100`). This experiment's own numbers come from its arm (`examples/experiment_305.rs`), which also takes 100 samples per input x decoder x variant.

## Reproducing

`mise run experiment 305` regenerates `raw/before.csv`, `raw/head.csv`, `raw/profile-head.txt` and `run.json`. It needs the `third_party/libjpeg-turbo` submodule and a C toolchain for the oracle. Its stdout prints the first table's medians and ratios, except the `#895 / zune` column, which comes from the first run's `raw/head.csv` at `1575b0db`. The paired table is the per-sample gamut / zune-jpeg ratio over the same rows (rows with equal `input` and `sample` in one CSV). `EXPERIMENT_SAMPLES`, `EXPERIMENT_ROUNDS`, `EXPERIMENT_WIDTH` and `EXPERIMENT_HEIGHT` override the defaults (100, 10, 6000, 4000).
