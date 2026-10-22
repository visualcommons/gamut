# Experiment #305: gamut-jpeg decode throughput against zune-jpeg

**Claim under test.** #305 measured `gamut-jpeg`'s decoder at 7-9x zune-jpeg's decode time on real camera JPEGs (24-45 MP). It named the Huffman/bit-reader hot path, the IDCT, and per-MCU allocation as the likely causes. This experiment measures the gap in-repo, profiles where the time goes, and measures what the decoder work bought.

**Acceptance rule** (maintainer decision on #305):

- Timings are reported as ratios against zune-jpeg in the same session, at 100 or more samples.
- Decoded output is still checked against libjpeg-turbo.
- If the gap does not close in one pass, the measured residual is filed.

**This record has four runs.**

1. The first ran on #895 (the entropy and presentation work that closed #305). Its data is in git at the commit that merged #895 (`1575b0db`).
2. The second re-ran the same script after #896 replaced the decoder's `f64` reference IDCT with `gamut-dsp`'s fixed-point `idct8x8_int`. Its data is in git at the commit that merged #898 (`7da7b2f4`).
3. The third re-ran it after #897's entropy-decoder work: a fused run/size/magnitude Huffman probe and a four-byte bit-reader refill. Its data is in git at the commit that merged #904 (`bf1125f3`).
4. The fourth, recorded in `raw/` and `run.json` now, re-ran it after #905's presentation work: the native layout written straight into the returned image, and the upsampling and YCbCr-to-RGB rows reshaped so the compiler vectorises them.

The earlier runs' numbers are quoted below where they are compared.

**Verdict: the presentation work narrowed the gap again without changing an output byte; the entropy decoder is now the clear majority of what is left.**

- **Gap to zune-jpeg.** Paired per sample, gamut now takes 2.2-2.8x zune-jpeg's time on baseline streams and 1.7x on the progressive one. After #897 it was 2.6-3.1x and 2.0x. After #896 it was 3.1-3.7x and 2.1x. After #895 it was 7.2-9.5x and 3.8x. Before #305 it was 8.7-11.3x and 4.1x. This run was measured under about five times the third run's load, and that load inflates gamut's time more than zune-jpeg's (the unchanged `before` decoder's paired ratio rose 7-12% between the two runs), so the step from the third run is if anything understated. The output-preserving claim does not depend on it.
- **Against scalar libjpeg-turbo.** gamut now takes 0.95-1.3x the time of the oracle build (`-DWITH_SIMD=0`). On `baseline-420` it is the faster of the two.
- **Output.** Every `head` fingerprint equals the third run's, on all four streams. The presentation work changed no decoded sample.
- **Residual.** Of gamut's own decode time, the entropy decoder is now 58%, the IDCT 25% and presentation 17% (it was 21%). What is left is listed below.

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
  - gamut's presentation writes `Rgb8` from a YCbCr stream straight into the image it returns (since #905; until the third run it copied the whole image once more through `gamut_core::convert`);
  - the oracle binding copies once out of libjpeg-turbo's C allocation;
  - zune-jpeg returns the buffer it decoded into.
- **Variants.** `before` is `crates/gamut-jpeg/src` at the commit before #305's work (`before_sha` in `run.json`), swapped in for one build. `head` is the tree as committed (`git_sha` in `run.json`). Everything else is identical between the two builds: the arm, the corpus, the other decoders, and `gamut-dsp`. `before` calls the `f64` `idct8x8`, which is unchanged, so it still measures the pre-#305 decoder. `head` now includes #896's kernel, #897's entropy work and #905's presentation work.
- **Sampling.** 100 timed decodes per input x decoder x variant. Both binaries are built first, then run in ten alternating rounds (before, head, before, head, ...) of ten samples each, with one untimed warm-up per input x decoder per round. Within a sample the three decoders are timed round-robin. A change in load during the session therefore lands on both variants and all three decoders alike. The machine was shared with other work throughout. `run.json` records the load average at the start and the end: 31.3 at the start and 32.6 at the end, on 16 threads (the third run's were 2.9 and 7.5, the second run's 13.3 and 29.5). That is why the ratios, not the absolute milliseconds, are the result. The absolute times are not comparable between runs.
- **Output check.** Every row carries an FNV-1a fingerprint of the decoded samples and the max/mean absolute difference from libjpeg-turbo's decode.

## Results

**Medians of 100 decodes, 6000x4000, fourth run.** Lower is better. "before / zune" and "head / zune" are each variant's median over the median of the zune-jpeg decodes timed in the same rounds. "Speed-up" is before over head. "#895 / zune", "#896 / zune" and "#897 / zune" are the first, second and third runs' head ratios: the decoder with #895's work and the `f64` IDCT, then with #896's integer IDCT, then with #897's entropy work.

| stream | bytes | gamut before | gamut head | zune-jpeg | libjpeg-turbo | before / zune | #895 / zune | #896 / zune | #897 / zune | head / zune | speed-up |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline-420` | 4,127,399 | 935 ms | 215 ms | 96 ms | 228 ms | 9.57x | 7.31x | 3.32x | 2.62x | 2.23x | 4.35x |
| `baseline-422` | 4,610,025 | 1235 ms | 275 ms | 103 ms | 229 ms | 10.88x | 8.39x | 3.70x | 2.97x | 2.67x | 4.49x |
| `baseline-444` | 6,125,769 | 1675 ms | 388 ms | 143 ms | 323 ms | 12.03x | 9.69x | 3.88x | 3.06x | 2.73x | 4.31x |
| `progressive-420` | 3,610,433 | 1129 ms | 443 ms | 270 ms | 362 ms | 4.53x | 3.78x | 2.16x | 1.97x | 1.64x | 2.55x |

(zune-jpeg and libjpeg-turbo columns: the `head` rounds' medians. The `before` rounds' are within 16% of them, the widest gap on `baseline-422`: this session's load moved more between rounds than the third run's, when they were within 2%. The speed-up is larger than the third run's 2.1-3.4x partly for the same reason the `before / zune` column rose: the load slows the older decoder most.)

**The same ratio, paired per sample.** In every sample, gamut and zune-jpeg decoded the same stream back to back in one process. Their ratio sample by sample is therefore the measurement least exposed to the machine's load. Median, with the 10th-90th percentile:

| stream | samples | gamut / zune-jpeg, before | #895 (first run) | #896 (second run) | #897 (third run) | head | gamut vs libjpeg-turbo, max / mean | zune-jpeg vs libjpeg-turbo, max / mean |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- |
| `baseline-420` | 100 | 9.39x (7.56-12.31) | 7.16x (6.49-8.10) | 3.12x (2.16-4.76) | 2.63x (2.59-2.74) | 2.15x (1.84-2.88) | 7 / 0.0504 | 4 / 0.0416 |
| `baseline-422` | 100 | 10.52x (8.73-14.32) | 8.39x (7.11-10.27) | 3.60x (2.53-4.91) | 2.98x (2.94-3.06) | 2.60x (2.20-3.36) | 6 / 0.0709 | 4 / 0.0482 |
| `baseline-444` | 100 | 11.70x (7.99-15.11) | 9.46x (8.20-10.82) | 3.70x (2.72-5.94) | 3.05x (3.02-3.18) | 2.84x (2.14-3.64) | 0 / 0.0000 | 3 / 0.0094 |
| `progressive-420` | 100 | 4.44x (3.09-6.05) | 3.78x (3.33-4.84) | 2.07x (1.58-3.23) | 1.98x (1.93-2.01) | 1.74x (1.36-2.17) | 7 / 0.0504 | 4 / 0.0416 |

(This run's percentiles interpolate linearly between ranks; the earlier columns are quoted as their runs recorded them. Re-derived the same way, the third run's medians are identical and its spreads within 0.07x.)

How to read it:

- **The output did not move.** Every `head` fingerprint (`output_fnv64` in `raw/head.csv`) equals the third run's, and so does every difference from libjpeg-turbo. #905 changes where the presented samples are written and how the loops that compute them are shaped; the arithmetic is still `gamut-color`'s `ycbcr_to_rgb`, sample for sample, and the upsampling still replicates. The second run's change is still the one that moved the output: #896's kernel is libjpeg-turbo's accurate integer IDCT bit for bit, so 4:4:4 decodes identically to libjpeg-turbo (max 0), and on subsampled streams the 6-7 code max difference left is chroma upsampling (gamut replicates chroma samples; libjpeg-turbo interpolates them).
- **How much of the step is #905's.** This run was measured under a load average of 31-33, the third under 3-8, and the load did not land on the decoders alike: the unchanged `before` decoder's paired ratio rose 7-12% between the two runs (8.66x to 9.39x on `baseline-420`, 10.46x to 11.70x on `baseline-444`). If the load costs the current decoder in the same proportion, the 7-18% step from the third run's `head` to this one's understates the change. The p10-p90 spreads are seven to ten times the third run's for the same reason. Every claim here is a median, and the output claim does not depend on the load at all.
- **Where #905 bought most.** The subsampled baseline streams moved most (2.63x to 2.15x on `baseline-420`), where presentation also upsamples the chroma rows; `baseline-444` moved least (3.05x to 2.84x), where only the conversion and the copy changed. Progressive moved from 1.98x to 1.74x.
- **How much of the third run's step was #897's.** The second run was measured under a load average of 13-30, the third under 3-8. The same `before` decoder's paired ratio fell 6.5% between them on `baseline-420` (9.26x to 8.66x), so up to about that much of the 16% step from 3.12x to 2.63x may be the machine, not the code. Within #904, an intermediate run at `ebcf473` measured the fused probe alone in a session of similar load (`raw/fused-probe-only/`): 3.01x, 3.37x, 3.51x and 2.23x paired. The refill change took those to the `head` column. That run's profile is the reason for the refill change: with the bit reader topped up on every call, `BitReader::fill` alone was 10.8% of all samples.
- **Progressive is still the closest shape and the least changed by entropy work.** Its refinement scans (`decode_ac_refine`) still decode one symbol per probe and read correction bits one at a time, and they are most of the progressive stream's entropy work. zune-jpeg's progressive decode costs 2.8x its baseline one (270 against 96 ms); gamut's costs 2.1x (443 against 215 ms).

## Where the time goes

`raw/profile-head.txt` is a flat `perf` profile of the `head` build: three samples per input x decoder, with all three decoders and the corpus generator in one process. The table shows the symbols that belong to gamut's decode, as a share of all samples and of gamut's own. The earlier runs' shares of gamut's decode are given for comparison:

| part | symbols | of all samples | of gamut's decode | third run | second run | first run |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| IDCT (`gamut-dsp`) | `idct8x8_int` 7.79% | 7.79% | 25.3% | 23.8% | 19.2% | 69.5% |
| entropy decode | `decode_internal` 4.59% (the progressive scan loops are inlined there), `BitReader::fill` 4.42%, `Ctx::decode_block` 4.23%, `decode_coded` 3.41%, `resolve` 1.23% | 17.88% | 58.1% | 54.7% | 62.0% | 23.8% |
| presentation (upsample + YCbCr to RGB) | `present_native` 4.09%, `assemble_progressive` 1.01% | 5.10% | 16.6% | 21.5% | 18.8% | 6.8% |

Presentation now has a symbol of its own, `present_native`, where the third run's was inlined into `DecodeImage<Rgb8>::decode_image` together with the `gamut_core::convert` copy that no longer runs for this request. gamut's decode is 30.8% of all samples, against 30.4% in the third run; that share also moves with how the load treats the other two decoders, so only the split within gamut's decode is compared. The corpus generator's own symbols are excluded (`experiment_305::main`, and libjpeg-turbo's `forward_DCT`/`encode_*`).

## What is left, and where it is filed

- **The entropy decoder (`gamut-jpeg`).** It is now well over half of gamut's decode, spread over the block loop, the progressive scan loops and the reader, with no single hot symbol. The progressive refinement path is the clearest target. The remaining gap is the standing goal of #169 ("Have the fastest JPEG decoder in Rust"); no narrower issue is filed for it yet.
- **The IDCT itself.** `idct8x8_int` is scalar. zune-jpeg's AVX2 IDCT (`idct_avx2` 1.25% + `idct_avx2_4x4` 0.54%) costs under a quarter of it per decode. SIMD variants behind the kernel's signature are already a `gamut-dsp` follow-up in its `STATUS.md`.
- **Presentation.** #905 removed the second full-image copy for a request in the stream's native layout and reshaped the upsampling and YCbCr-to-RGB loops so the compiler vectorises them with the baseline instruction set (SSE2 on x86-64), without `unsafe`. zune-jpeg's conversion still costs a fraction of gamut's (`ycbcr_to_rgb_avx2_1` 0.89%), because it runs 16-bit arithmetic on AVX2: gamut's stays 32-bit, which is what keeps it byte-identical to `ycbcr_to_rgb`, and AVX2 itself would need runtime dispatch, which on x86 needs `unsafe`.

## Scope of #305 and the harness choice

The [delivery record on #305](https://github.com/visualcommons/gamut/issues/305#issuecomment-6236460699) records two departures from the issue's original decision:

- **Scope.** #305 is reduced to the bench, this experiment record and this first pass. The reduction is the maintainer's decision of 2026-10-17, which supersedes the 2026-10-16 note that an unclosed gap lands as `Part of #305`: "Close #305, residual in #896/#897 — #305 becomes 'bench + experiment + first pass', the gap work lives in #896/#897". The residual gap is not part of #305: it is filed as #896 (the `gamut-dsp` IDCT, which this record's second run measures) and #897 (the `gamut-jpeg` follow-up, which the third run measures), whose presentation remainder #905 delivered (the fourth run).
- **Harness.** The issue's decision named criterion with at least 100 samples. The bench (`crates/gamut-jpeg/benches/decode.rs`) uses divan instead, following the workspace's single-harness convention (#149, `docs/benchmarking.md`). It keeps the 100 samples per stream shape (`sample_count = 100`). This experiment's own numbers come from its arm (`examples/experiment_305.rs`), which also takes 100 samples per input x decoder x variant.

## Reproducing

`mise run experiment 305` regenerates `raw/before.csv`, `raw/head.csv`, `raw/profile-head.txt` and `run.json`. It needs the `third_party/libjpeg-turbo` submodule and a C toolchain for the oracle. Its stdout prints the first table's medians and ratios, except the `#895 / zune`, `#896 / zune` and `#897 / zune` columns, which come from the first, second and third runs' `raw/head.csv` at `1575b0db`, `7da7b2f4` and `bf1125f3`. `raw/fused-probe-only/` holds the intermediate run at `ebcf473` (its own `run.json`, `before.csv`, `head.csv` and `profile-head.txt`); the script does not regenerate it, and it is reproduced by checking out `ebcf473` and running the same command. The paired table is the per-sample gamut / zune-jpeg ratio over the same rows (rows with equal `input` and `sample` in one CSV). `EXPERIMENT_SAMPLES`, `EXPERIMENT_ROUNDS`, `EXPERIMENT_WIDTH` and `EXPERIMENT_HEIGHT` override the defaults (100, 10, 6000, 4000).
