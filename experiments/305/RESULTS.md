# Experiment #305: gamut-jpeg decode throughput against zune-jpeg

**Claim under test.** #305 measured `gamut-jpeg`'s decoder at 7-9x zune-jpeg's decode time on real camera JPEGs (24-45 MP). It named the Huffman/bit-reader hot path, the IDCT, and per-MCU allocation as the likely causes. This experiment measures the gap in-repo, profiles where the time goes, and measures what this change's decoder work bought.

**Acceptance rule** (maintainer decision on #305):

- Timings are reported as ratios against zune-jpeg in the same session, at 100 or more samples.
- Decoded output is still checked against libjpeg-turbo.
- If the gap does not close in one pass, the measured residual is filed.

**Verdict: a partial closure, with the residual filed.**

- **Speed-up.** This change makes `gamut-jpeg`'s decode 1.14-1.26x faster on every stream shape, with byte-identical output.
- **Gap to zune-jpeg.** The gap moves from 8.7-11.3x to 7.3-9.7x on baseline streams, and from 4.1x to 3.8x on the progressive one.
- **Residual.** About 70% of what remains is one kernel: `gamut-dsp`'s `f64` reference IDCT, which rebuilds its cosine table on every block. Most of the rest is the entropy decoder. Both are filed: #896 for the IDCT, and #897 for the `gamut-jpeg` follow-up, blocked by #896.

## Method

- **Corpus.** `crates/gamut-jpeg/benches/common/corpus.rs` generates deterministic photo-like RGB at 6000x4000 (24 MP, the small end of the issue's range):
  - a smooth separable illumination field, standing in for most of a photograph's area;
  - per-pixel noise from a fixed-seed LCG, ±8 codes per channel, which keeps AC coefficients alive at q90 the way sensor noise does;
  - hard-edged flat rectangles on a coarse grid.

  libjpeg-turbo (the crate's oracle, `tooling/libjpeg-oracle`) encodes it at q90 with its standard tables into four streams: baseline 4:2:0, 4:2:2 and 4:4:4, and progressive 4:2:0 (its standard ten-scan script). Neither decoder under test wrote the streams. The streams' SHA-256s are in `run.json`.
- **Decoders.**
  - `gamut`: `JpegDecoder::decode_image` to `Rgb8`.
  - `zune-jpeg 0.5.15`: default features, x86 SIMD on, to RGB.
  - `libjpeg-turbo 3.2.0`: the vendored oracle build, to RGB. It is built `-DWITH_SIMD=0` (`tooling/libjpeg-oracle/build.rs`), so it is a scalar-C yardstick, not libjpeg-turbo's shipped SIMD speed.

  Each decode is one wall-clock call, including upsampling and colour conversion. Each timing includes the copies its decoder makes on the way out:
  - gamut's presentation copies once through `gamut_core::convert`, which every caller pays;
  - the oracle binding copies once out of libjpeg-turbo's C allocation;
  - zune-jpeg returns the buffer it decoded into.
- **Variants.** `before` is `crates/gamut-jpeg/src` at the base commit (`before_sha` in `run.json`), swapped in for one build. `head` is this change. Everything else (the arm, the corpus, the other decoders) is identical between them.
- **Sampling.** 100 timed decodes per input x decoder x variant. Both binaries are built first, then run in ten alternating rounds (before, head, before, head, ...) of ten samples each, with one untimed warm-up per input x decoder per round. Within a sample the three decoders are timed round-robin. A change in load during the session therefore lands on both variants and all three decoders alike. The machine was shared with other work throughout (`run.json` records the load average at the start and the end). That is why the ratios to zune-jpeg and the before/head speed-up, not the absolute milliseconds, are the result.
- **Output check.** Every row carries an FNV-1a fingerprint of the decoded samples and the max/mean absolute difference from libjpeg-turbo's decode.

## Results

**Medians of 100 decodes, 6000x4000.** Lower is better. "before / zune" and "head / zune" are each variant's median over the median of the zune-jpeg decodes timed in the same rounds. "Speed-up" is before over head. "Output" compares the FNV-1a fingerprints of the two variants' decoded samples.

| stream | bytes | gamut before | gamut head | zune-jpeg | libjpeg-turbo | before / zune | head / zune | speed-up | output |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `baseline-420` | 4,127,399 | 762 ms | 605 ms | 83 ms | 192 ms | 8.66x | 7.31x | 1.26x | identical |
| `baseline-422` | 4,610,025 | 923 ms | 766 ms | 91 ms | 195 ms | 10.11x | 8.39x | 1.20x | identical |
| `baseline-444` | 6,125,769 | 1280 ms | 1095 ms | 113 ms | 243 ms | 11.30x | 9.69x | 1.17x | identical |
| `progressive-420` | 3,610,433 | 895 ms | 783 ms | 207 ms | 276 ms | 4.06x | 3.78x | 1.14x | identical |

(zune-jpeg and libjpeg-turbo columns: the `head` rounds' medians. The `before` rounds' are within 7% of them: 88/91/113/221 ms for zune-jpeg, and 191/195/249/279 ms for libjpeg-turbo.)

**The same ratio, paired per sample.** In every sample, gamut and zune-jpeg decoded the same stream back to back in one process, so their ratio sample by sample is the measurement least exposed to the machine's load. Median, with the 10th-90th percentile:

| stream | samples | gamut / zune-jpeg, before | gamut / zune-jpeg, head | gamut vs libjpeg-turbo, max / mean | zune-jpeg vs libjpeg-turbo, max / mean |
| --- | ---: | --- | --- | --- | --- |
| `baseline-420` | 100 | 8.90x (7.21-11.33) | 7.15x (6.51-8.09) | 7 / 0.0960 | 4 / 0.0416 |
| `baseline-422` | 100 | 10.10x (9.45-11.35) | 8.39x (7.12-10.26) | 6 / 0.1026 | 4 / 0.0482 |
| `baseline-444` | 100 | 10.97x (10.06-12.88) | 9.46x (8.21-10.81) | 3 / 0.0331 | 3 / 0.0094 |
| `progressive-420` | 100 | 4.07x (3.64-4.69) | 3.77x (3.34-4.84) | 7 / 0.0960 | 4 / 0.0416 |

How to read it:

- **The output did not move.** Both variants' fingerprints are identical on every stream, so the speed-up is the same computation done faster. gamut's difference from libjpeg-turbo is unchanged:
  - 3 codes max on 4:4:4, the IDCT rounding the oracle gate already bounds;
  - 6-7 codes max on subsampled streams, where gamut replicates chroma samples and libjpeg-turbo interpolates them ("fancy" upsampling);
  - the mean difference stays under 0.11 codes.
- **Progressive is the closest shape.** Progressive and baseline 4:2:0 encode the same image. zune-jpeg's progressive decode costs 2.5x its baseline one (207 against 83 ms); gamut's costs 1.3x (783 against 605 ms). gamut spends most of its time in the IDCT, which runs once per block either way: the progressive path fills a coefficient buffer across scans and transforms it once at the end.
- **The run was noisy.** The machine ran other jobs throughout (load average 29.4 at the start, 22.7 at the end, on 16 threads). The p10-p90 spread above is mostly that noise. Per round, the before/head speed-up ranged from 0.89x to 2.22x around the medians above. Ten alternating rounds keep it off the comparison between variants, but it is why every claim here is a median.

## Where the time goes

`raw/profile-head.txt` is a flat `perf` profile of the `head` build: three samples per input x decoder, with all three decoders and the corpus generator in one process. The symbols that belong to gamut's decode, as a share of all samples and of gamut's own:

| part | symbols | of all samples | of gamut's decode |
| --- | --- | ---: | ---: |
| IDCT (`gamut-dsp`) | `__cos_fma` 23.94%, `idct8x8` 11.40%, `round` 5.04% | 40.38% | 69.5% |
| entropy decode | `decode_symbol` 5.55%, `Ctx::decode_block` 5.22%, `decode_internal` 2.46% (the progressive scan loops are inlined there), `BitReader::fill` 0.59% | 13.82% | 23.8% |
| presentation (upsample + YCbCr to RGB + convert) | `DecodeImage<Rgb8>::decode_image` 3.57%, `assemble_progressive` 0.37% | 3.94% | 6.8% |

`__cos_fma` is the `cos()` that `idct8x8`'s `cosine_table()` calls, 64 times per block. `round` is its `f64::round`. Neither comes from anywhere else in gamut's decode path. The corpus generator's own symbols (`experiment_305::main`, libjpeg-turbo's `forward_DCT`/`encode_*`) are excluded. Only the `head` build is profiled: the speed-up itself is in the timing table, and this profile shows where the time that remains goes.

## What is left, and where it is filed

- **The IDCT (#896, `gamut-dsp`).** `idct8x8` evaluates the §A.3.3 ideal IDCT in `f64` and rebuilds its cosine table (64 `cos()` calls) on every block; its module doc says it targets clarity over speed. That dominates what is left. Its fix belongs to the crate that owns the kernel, so this change does not work around it in the decoder. A decoder-side DC-only shortcut (a flat block of `round(S00/8)`) would be such a workaround: a second copy of the kernel's arithmetic that #896's fix would not reach. #896 asks for the sparsity hint to cross the kernel's API instead.
- **The entropy decoder and presentation (#897, `gamut-jpeg`, blocked by #896).** This covers adopting the new kernel and a fused AC run/size + magnitude fast path. It also covers re-running this experiment. zune-jpeg's `ac_lookup` and four-byte refill are the reference points.

## Reproducing

`mise run experiment 305` regenerates `raw/before.csv`, `raw/head.csv`, `raw/profile-head.txt` and `run.json`. It needs the `third_party/libjpeg-turbo` submodule and a C toolchain for the oracle. Its stdout prints the first table's medians and ratios. The paired table is the per-sample gamut / zune-jpeg ratio over the same rows (rows with equal `input` and `sample` in one CSV). `EXPERIMENT_SAMPLES`, `EXPERIMENT_ROUNDS`, `EXPERIMENT_WIDTH` and `EXPERIMENT_HEIGHT` override the defaults (100, 10, 6000, 4000).
