# Experiment #30: encoder techniques from jpegli, libjpeg-turbo and mozjpeg

**Question.** #30 asks whether other high-quality JPEG encoders (jpegli, libjpeg-turbo) use
techniques gamut-jpeg has not explored. #29 and #974 already measured quantization tables, the AC
and DC trellis and overshoot deringing (`experiments/29/RESULTS.md`,
`experiments/974/RESULTS.md`). This record surveys what is left in those encoders and measures the
five techniques that gamut-jpeg does not have and that can be added without a new dependency:

| patch | source | what it changes |
| --- | --- | --- |
| `zero-bias` | jpegli, `lib/jpegli/quant.cc` (`kZeroBiasOffsetYCbCrAC`) and `encode_finish.cc` (`QuantizeBlock`) | YCbCr AC coefficients whose magnitude is below 0.59082 / 0.58146 / 0.57988 of a step (Y / Cb / Cr) quantize to 0 instead of rounding to 1. DC and other coefficients round to nearest as before |
| `downsample-bias` | libjpeg-turbo, `jcsample.c` (`h2v1_downsample`, `h2v2_downsample`) | the 2:1 chroma box filter rounds with libjpeg's alternating bias (1, 2, 1, 2, ... for 2x2; 0, 1, 0, 1, ... for 2x1) instead of always rounding half up |
| `smooth-10` | libjpeg-turbo, `jcsample.c` (`fullsize_smooth_downsample`, `h2v2_smooth_downsample`) | input smoothing at `smoothing_factor` 10 (`cjpeg -smooth 10`) on luma and chroma before the FDCT |
| `scans-mozjpeg` | mozjpeg, `jcparam.c` (`jpeg_simple_progression` under `JCP_MAX_COMPRESSION`, `dc_scan_opt_mode = 0`) | the progressive scan script: DC sent whole, luma AC split at 8/9, chroma AC sent whole in two bands. 5 scans for grey, 9 for YCbCr |
| `scans-jpegli` | jpegli, `lib/jpegli/encode.cc` (`SetDefaultScanScript`, `progressive_level = 2`) | the progressive scan script: one DC scan (interleaved only with no subsampling), then four AC passes per component. 5 scans for grey, 13 for 4:4:4, 15 for 4:2:0 |

Each technique is a patch of `crates/gamut-jpeg/src`. It is applied for one build of a
measurement arm and reverted afterwards, as the #514, #612 and #974 records do. Nothing under
`crates/gamut-jpeg/src` changes on master.

**Verdict: no shipped default changes. One technique is a rate-only win worth an opt-in, filed
as #1021. One is a cheaper, weaker alternative to the shipped trellis. Two cost rate or do
nothing.**

- **`scans-mozjpeg`: smaller on every image and configuration, by 0.35-1.51% on average.** A scan
  script changes how the same quantized coefficients are coded, never the coefficients
  themselves. The arm asserts this at every point: libjpeg-turbo decodes both streams to
  identical pixels. So this saving costs no fidelity. Averaged over the seven images, the stream
  is 0.66% smaller for 4:2:0, 1.51% for 4:4:4 and 0.35% for grey. It is smaller in all 21 image x
  configuration cells, by 0.13% to 2.87%. The shipped script is SemVer-frozen (`STATUS.md`,
  *Progressive scan script (frozen)*), and mozjpeg is not vendored under `references/`. Shipping
  this script therefore needs an API decision (an opt-in, or a breaking change of the frozen
  script) and a vendored citation. #1021 tracks both.
- **`zero-bias`: -3% to -6% at equal fidelity on every plain YCbCr variant, but 0.8-1.8% behind the
  shipped trellis.** The patch saves 3.8% (PSNR-Y), 4.8% (PSNR-RGB) and 3.1% (SSIM-Y) on `annexk`,
  and up to 6.4% on the other plain variants. It does not touch the trellis path or XYB (jpegli
  applies it to YCbCr only), and those variants are byte-identical to the baseline build. The two
  are alternatives on one path, so the useful comparison is with the shipped trellis on the same
  tables. Against `trellis`, zero-biased `annexk` is 1.10% / 0.79% / 0.81% larger at equal
  PSNR-Y / PSNR-RGB / SSIM-Y. Against `luma-csf32-trellis`, zero-biased `luma-csf32` is 1.79% /
  1.44% / 1.35% larger. The zero bias costs one compare per coefficient where the trellis runs a
  dynamic program per block. But the default path is byte-frozen, and the trellis already ships
  as the opt-in that buys this rate. So the measured evidence does not support a second, weaker
  opt-in.
- **`smooth-10`: costs rate, as it should on clean input.** It loses 3.05% (PSNR-Y) on `annexk` and
  5.37% on `trellis`, and up to 17% on the text image. Smoothing exists to suppress dither and
  film-grain noise before the DCT. This corpus has none, and fidelity is measured against the
  unsmoothed source, so these metrics can only charge for the blur. A noisy-input corpus would be
  needed to show a gain. Nothing to ship.
- **`downsample-bias`: negligible.** On 4:2:0 it moves PSNR-RGB by -0.14% to -0.28%. Luma and SSIM
  move by under 0.03%, except on `testorig`, the recompressed JPEG (#29), where two single cells
  move 3-4% in opposite directions (below). 4:4:4 and XYB, which have no 2:1 box, are
  byte-identical, which confirms the patch is confined to the box filter. Nothing to ship.

## Survey

What jpegli and libjpeg-turbo do that gamut-jpeg did not already have or measure, and where each
item went. libjpeg-turbo is vendored (`third_party/libjpeg-turbo`). jpegli is not: it was read
from libjxl 0.11's `lib/jpegli` (crate `jpegxl-src` 0.11.4). libjxl 0.12.0, which `gamut-jxl-sys`
builds, no longer carries that directory. mozjpeg is not vendored. Its
scan script is transcribed from its published `jcparam.c` and was not diffed against a local copy.

| technique | encoder | disposition |
| --- | --- | --- |
| non-adaptive AC zero bias | jpegli | measured (`zero-bias`) |
| adaptive quantization field (per-block zero-bias multiplier from local masking) | jpegli | not measured: needs jpegli's masking model, and its gain can only be judged with a perceptual metric (*What is left*) |
| XYB colour mode, B-only subsampling, XYB tables | jpegli | shipped (P13), deferred in `STATUS.md`, or measured by #29 |
| default progressive script | jpegli | measured (`scans-jpegli`) |
| ordered rounding bias in the 2:1 box filter | libjpeg-turbo | measured (`downsample-bias`) |
| input smoothing (`-smooth`) | libjpeg-turbo | measured (`smooth-10`) |
| optimized Huffman tables | libjpeg-turbo | shipped (P10, `with_optimized_tables`) |
| `jpeg_simple_progression` | libjpeg-turbo | shipped, frozen (P5) |
| arithmetic coding | libjpeg-turbo | out of scope here: a different entropy coder (T.81 Annex D) that much deployed software cannot decode. It is a format feature, not an encoder trick |
| trellis quantization, DC trellis, overshoot deringing | mozjpeg | shipped (P12) or measured (#974) |
| `JCP_MAX_COMPRESSION` scan script | mozjpeg | measured (`scans-mozjpeg`) |
| scan optimization (`optimize_scans`: try several scripts, keep the smallest) | mozjpeg | not measured: the fixed script above is its starting point; see *What is left* |

## Results

### Quantization patches

Mean rate difference of each patched build against the same variant in the baseline build, over
the seven images, in percent. Negative means smaller at equal fidelity. `raw/bdrate-*.csv` holds
every per-image value with its overlap range.

| variant | `zero-bias` PSNR-Y | PSNR-RGB | SSIM-Y | `smooth-10` PSNR-Y | PSNR-RGB | SSIM-Y | `downsample-bias` PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `annexk` | -3.81 | -4.82 | -3.06 | +3.05 | +1.18 | +0.52 | +0.01 | -0.28 | -0.00 |
| `annexk-444` | -4.04 | -5.64 | -4.36 | +3.52 | +1.95 | +0.38 | 0.00 | 0.00 | 0.00 |
| `trellis` | 0.00 | 0.00 | 0.00 | +5.37 | +3.38 | +2.04 | -0.01 | -0.25 | -0.00 |
| `trellis-adaptive` | 0.00 | 0.00 | 0.00 | +5.27 | +3.35 | +2.13 | -0.51 | -0.27 | -0.02 |
| `luma-flat` | -4.93 | -6.43 | -3.66 | +2.55 | +0.71 | +0.17 | -0.00 | -0.14 | -0.01 |
| `luma-csf32` | -4.94 | -6.38 | -4.07 | +2.95 | +1.13 | +0.17 | -0.00 | -0.17 | -0.01 |
| `luma-csf64` | -3.30 | -4.89 | -3.00 | +3.78 | +1.92 | +0.94 | -0.01 | -0.24 | -0.01 |
| `luma-csf32-trellis` | 0.00 | 0.00 | 0.00 | +4.94 | +3.14 | +1.59 | +0.01 | -0.14 | +0.01 |
| `chroma-x0.5` | -3.15 | -5.21 | -3.38 | +3.50 | +1.64 | +0.54 | +0.62 | -0.20 | +0.00 |
| `chroma-x2` | -3.17 | -4.66 | -2.92 | +3.45 | +1.24 | +1.06 | 0.00 | -0.21 | -0.02 |

Every XYB variant is 0.00 in all three builds: none of the three patches reaches the XYB path.
`raw/changed.csv` confirms it, with 0 of 98 points changed for each XYB variant. It also shows 0
for the trellis variants under `zero-bias`, and 0 for `annexk-444` under `downsample-bias`.

**`zero-bias` against the shipped trellis** (`raw/bdrate-zero-bias-vs-trellis.csv`; positive means
the zero-biased plain variant is larger than the trellis variant at equal fidelity):

| zero-biased variant | against | PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | --- | ---: | ---: | ---: |
| `annexk` | `trellis` | +1.10 | +0.79 | +0.81 |
| `luma-csf32` | `luma-csf32-trellis` | +1.79 | +1.44 | +1.35 |

**`zero-bias` on `annexk`, per image:**

| metric | `testorig` | `shira_bird8` | `vgl_6434_0018a` | `vgl_6548_0026a` | `synthetic-512x384` | `clipped-512x384` | `text-384x256` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| PSNR-Y | -6.99 | -2.91 | -2.32 | -0.46 | -8.32 | -5.68 | +0.02 |
| PSNR-RGB | -1.44 | -2.26 | +0.50 | -0.46 | -15.81 | -14.29 | +0.02 |
| SSIM-Y | -3.26 | -1.59 | -4.90 | -0.60 | -6.01 | -3.14 | -1.93 |

How to read it:

- **The zero bias acts on every point and pays most on the synthetic images.** It changed the
  stream size on 96-98 of each plain YCbCr variant's 98 points. Its largest savings are on the two
  generated photo-like images (-8% to -16%). A plausible reading, not measured, is that their
  generator's fine texture leaves many AC coefficients just above half a step, where the bias
  moves them to zero. The one regression in
  the table is `vgl_6434_0018a` at equal PSNR-RGB (+0.50%), on a variant whose PSNR-Y and SSIM-Y
  for that image both improve.
- **`text-384x256` barely moves** (+0.02% PSNR, -1.9% SSIM). Its blocks are mostly flat white or
  hard ink edges, whose AC coefficients are far from half a step.
- **The trellis subsumes the zero bias.** The trellis already chooses `v - 1` (including 0) for a
  coefficient whenever the bits saved outweigh the distortion added. A fixed threshold is a
  special case of that choice, made without the rate. That fits the trellis staying 0.8-1.8%
  ahead, but it is an explanation, not a measurement.
- **`smooth-10`'s cost is concentrated on hard edges.** `annexk` loses 16.2% on the text image and
  7.0% on the neutral-grey `vgl_6548_0026a`, and gains 0.8-1.9% on the three photo-like images
  (`synthetic`, `clipped`, `shira_bird8`). The trellis variants lose everywhere (+1.2% to +17.4%):
  the trellis already discards the high-frequency energy that smoothing removes, so smoothing
  then removes signal with no rate left to win back.
- **`downsample-bias`'s outliers are `testorig`.** The image is a recompressed JPEG (#29), with an
  irregular rate-distortion curve. Its PSNR-Y moves -3.44% on `trellis-adaptive` and +4.32% on
  `chroma-x0.5`. Every other image stays within 0.1% on those two variants.

### Scan scripts

Mean over the 14 qualities of the progressive stream's size, in percent, against the shipped
script's progressive stream (`vs shipped`) and against the sequential stream with optimized
tables (`vs sequential`). Negative is smaller. `raw/scans-summary.csv` has every image.

| script | config | scans | vs shipped | vs sequential |
| --- | --- | ---: | ---: | ---: |
| shipped (`jpeg_simple_progression`) | 4:2:0 | 10 | 0.00 | +0.34 |
| | 4:4:4 | 10 | 0.00 | +1.37 |
| | grey | 6 | 0.00 | -1.12 |
| `scans-mozjpeg` | 4:2:0 | 9 | -0.66 | -0.33 |
| | 4:4:4 | 9 | -1.51 | -0.18 |
| | grey | 5 | -0.35 | -1.48 |
| `scans-jpegli` | 4:2:0 | 15 | +0.42 | +0.76 |
| | 4:4:4 | 13 | -0.14 | +1.21 |
| | grey | 5 | -1.32 | -2.46 |

How to read it:

- **mozjpeg's script wins everywhere, by a little.** Its saving against the shipped script is
  -0.13% (`shira_bird8`, grey) to -2.87% (`synthetic-512x384`, 4:4:4). It is the only script here
  that makes the progressive stream smaller than the sequential one on average in every
  configuration. Most of its gain plausibly comes from dropping DC successive approximation, which
  saves the DC refinement scan and its header. That reading is untested.
- **jpegli's script splits by image size in colour.** At 4:2:0 it is 4.2-7.0% smaller than the
  shipped script on the two 512x384 images. It is +0.23% on the 384x256 text image, and 1.7-5.7%
  larger on the four small test images, which are 95-149 pixels tall. For grey (5 scans against
  6) it is smaller on every image but one (`text-384x256`, +0.45%). gamut writes a separate optimized Huffman table for each of the
  13-15 scans. jpegli itself clusters per-scan histograms into shared tables
  (`ClusterJpegHistograms`, `entropy_coding.cc`). So on a small image, gamut pays the
  per-scan table overhead that jpegli avoids. This is an explanation, not a measurement: the arm
  does not count DHT bytes. The script is not a candidate until table sharing across scans exists.

## Method

- **Builds.**
  - **Quantization patches:** four builds of #29's arm
    (`crates/gamut-jpeg/examples/experiment_29.rs`), first the tree as checked out (`baseline`)
    and then the tree with each patch applied. Each build runs #29's whole `measure` sweep, all 17
    encoder configurations ("variants": tables, subsampling, RD mode, colour mode; see
    `experiments/29/RESULTS.md`). A patched variant is compared with **the same variant in the
    baseline build**, so the only difference is the patch.
  - **Scan-script patches:** three builds of this record's arm
    (`crates/gamut-jpeg/examples/experiment_30.rs`). For each image, configuration (YCbCr 4:2:0,
    YCbCr 4:4:4, grey from BT.601 luma) and quality, the arm encodes the sequential stream with
    optimized tables and the progressive stream. It asserts that libjpeg-turbo decodes the two to
    identical pixels, and records both sizes and the scan count. Quantization is unchanged by
    construction, so the comparison is a plain size ratio, not a rate-distortion curve.
- **`zero-bias.patch`** (`encoder.rs`, `progressive.rs`). It adds a per-component threshold to the
  shared `quantize_block` seam, so baseline and progressive keep identical coefficients. Each YCbCr
  component gets jpegli's offset for that component, and grey and XYB get 0, which is the shipped
  rounding. The threshold is integer: an AC coefficient `c` with `|c| * 100000 < offset * step`
  becomes 0. This matches jpegli's `Ge(|c / step|, threshold)` keep-test, which uses jpegli's
  non-adaptive path (`use_adaptive_quantization` off). The RD path calls the trellis instead and
  is untouched.
- **`downsample-bias.patch`** (`encoder.rs`). The full 2x2 and 2x1 boxes in `downsample` add
  libjpeg's alternating bias by output column. Partial edge boxes keep the shipped half-up average.
- **`smooth-10.patch`** (`encoder.rs`). It transcribes libjpeg-turbo's two smoothing filters in
  their 16-bit fixed-point form. Edges are replicated as libjpeg's context rows and
  `expand_right_edge` do. Full-size planes (luma, and chroma at 4:4:4) take
  `fullsize_smooth_downsample`, and 2x2 chroma takes `h2v2_smooth_downsample`. Any other factor
  falls back to the plain box, since libjpeg has no smoothing filter for it.
- **`scans-*.patch`** (`progressive.rs`). Each replaces `scan_script` with the source's script.
  The jpegli script needs to know whether any component is subsampled, so its patch passes that
  in.
- **Corpus.** #974's seven inputs: four libjpeg-turbo test images (`testorig.ppm`,
  `shira_bird8.bmp`, `vgl_6434_0018a.bmp`, `vgl_6548_0026a.bmp`), the decode bench's 512x384
  synthetic, and the two saturated images `crates/gamut-jpeg/examples/experiment_974.rs`
  generates (`clipped-512x384`, `text-384x256`). SHA-256s are in `run.json`. The images are small,
  which is the corpus's main weakness, as #29 notes. It matters most for the scan scripts, whose
  fixed overhead is per scan rather than per pixel.
- **Decode, metrics, rate comparison.** As #29: libjpeg-turbo (the crate's oracle) decodes every
  stream. The metrics are PSNR on BT.601 luma, PSNR over RGB, and SSIM on luma. Rates are
  compared by a Bjontegaard-style delta over q = 30..95, from #29's `bdrate` subcommand, with
  negative meaning smaller at equal fidelity. `raw/changed.csv` counts, per patch, variant and
  image, the quality points whose stream size differs from the baseline build's.

## What is left

- **#1021: mozjpeg's scan script.** It needs a vendored citation and a decision between an opt-in
  and a breaking change of the frozen script.
- **jpegli's adaptive quantization.** jpegli's default mode scales the zero-bias threshold per
  block from a local masking estimate (`quant_field`, `zero_bias_mul`). Its purpose is
  perceptual, so it can only be judged with the perceptual-metric oracle #29 and #974 wait on.
- **Scan optimization** (mozjpeg's `optimize_scans`): encode several candidate scripts and keep
  the smallest. The fixed mozjpeg script is its starting point. A search would also cost several
  extra entropy passes per encode.
- **Table sharing across progressive scans.** jpegli's clustering of per-scan histograms into
  shared tables would remove the per-scan overhead that makes its script lose on small images.
- **Smoothing on noisy input.** `smooth-10` could only lose on this clean corpus. A dithered or
  grainy corpus would test what the filter is for.

## Reproducing

`mise run experiment 30` regenerates every file in `raw/` and `run.json`. It needs the
`third_party/libjpeg-turbo` submodule, for the oracle build and the test images, and a C
toolchain. It refuses to run while `crates/gamut-jpeg/src` has uncommitted changes, applies each
patch for one build, and reverts it on any normal or failed exit. A run killed by a signal can
leave a patch applied: `git apply -R experiments/30/<patch>.patch` undoes it. Its stdout prints
the mean tables above, the per-variant counts of changed points and the scan-script table.
Encodes are deterministic, so a re-run on the same `git_sha` reproduces `raw/` exactly, within
gamut-color's `f64` Tier-1 determinism for the XYB rows. `EXPERIMENT_SYNTHETIC` (default
`512x384`) sets the synthetic image's size.
