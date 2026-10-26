# Experiment #29: quantization choices gamut-jpeg's encoder does not make yet

**Question.** #29 asks for clean-slate experiments with JPEG encoder quality, kept as a record of
what was tried. This first record measures the quantization choices the shipped encoder does not
make, all reachable through its public API:

- alternate luminance base tables (flat; inverse contrast-sensitivity);
- a different luma/chroma balance;
- tables aimed at the XYB colour mode, whose Annex K pairing `STATUS.md` calls a placeholder.

The shipped Annex K path and the shipped trellis modes are the yardsticks.

**Verdict: nothing here justifies changing a shipped default yet. A perceptual metric (SSIMULACRA
2, issue #972) now scores every variant, and it overturns most of what PSNR and SSIM said. What it
points at, a finer chroma table, needs a larger corpus before it can move a default.**

- **Flat and CSF luma tables.** On PSNR and SSIM they beat Annex K by 12-14% at equal fidelity.
  But PSNR and SSIM are mean-squared-error-family metrics, and a flat table is the MSE-optimal
  shape for an orthonormal transform: they cannot credit what Annex K is for, coarse high
  frequencies where the eye does not see the error. On SSIMULACRA 2 the gain shrinks to 2.6%
  (`luma-flat`) and 4.1% (`luma-csf32`), and on two of the five images the flat table loses. The
  CSF table, not the flat one, is the better luma shape perceptually, and it still stacks with the
  trellis (-8.5%). The gain is small enough that this corpus cannot settle it.
- **Luma/chroma balance.** On PSNR and SSIM it is a trade, not a gain: halving the chroma steps
  costs 5% on the luma metrics and saves 5% on RGB PSNR, and doubling them does the reverse. On
  SSIMULACRA 2 it is not a trade. Halving the chroma steps saves 9.7% and doubling them costs 16%,
  so on this corpus the Annex K.2 table at 4:2:0 is too coarse for the eye. This is the largest
  perceptual gain in the record, and the candidate a follow-up should test on more images.
- **XYB.** On sRGB PSNR and SSIM, XYB costs far more rate than YCbCr 4:2:0 on every table tried:
  +69% with the shipped pairing. On SSIMULACRA 2 the gap is +16%, and it is a mean over images that
  disagree: XYB loses 23-43% on four of them and wins 40% on the saturated-colour
  `vgl_6434_0018a`, where 4:2:0 cannot get above a score of 32 even at q95. Within XYB the
  perceptual metric no longer backs the PSNR result: flat and CSF X/Y tables, the clearest PSNR
  gain (11-12%), cost 9% and 3.5% on the mean. That mean is carried by the synthetic image (+70%,
  +28%); on the four real images `xyb-flat` gains on three, so this is unsettled rather than
  reversed. Coarsening the B table costs 9-25%. `xyb-trellis` gains 3.9%.
  One limit is the encoder's own surface: X and Y share quantization destination 0, so the obvious
  next experiment, a coarser X than Y, cannot be expressed (see *What is left*).
- **Not attempted.** DC trellis and overshoot deringing need encoder internals that the public API
  does not expose (see *What is left*).

## Method

- **Variants.** Every variant is a configuration of the shipped `JpegEncoder`. All of them use
  `with_optimized_tables(true)`, so the Huffman tables fit each variant's coefficients and the
  comparison is between quantization choices. Alternate tables are built in the arm
  (`crates/gamut-jpeg/examples/experiment_29.rs`) and passed as
  `with_quant_tables(QuantTables::new(luma, chroma).scaled(q))`, so the frozen IJG quality mapping
  scales them exactly as it scales Annex K. Nothing under `crates/gamut-jpeg/src` is patched.
  `raw/tables.csv` lists every base table, in natural order.

  | variant | colour | sampling | RD | luma (dest 0) base | chroma (dest 1) base |
  | --- | --- | --- | --- | --- | --- |
  | `annexk` | YCbCr | 4:2:0 | none | K.1 | K.2 |
  | `annexk-444` | YCbCr | 4:4:4 | none | K.1 | K.2 |
  | `trellis` / `trellis-adaptive` | YCbCr | 4:2:0 | `Trellis` / `TrellisAdaptive` | K.1 | K.2 |
  | `luma-flat` | YCbCr | 4:2:0 | none | flat 45 | K.2 |
  | `luma-csf32` / `luma-csf64` | YCbCr | 4:2:0 | none | CSF at 32 / 64 px per degree | K.2 |
  | `luma-csf32-trellis` | YCbCr | 4:2:0 | `Trellis` | CSF at 32 px per degree | K.2 |
  | `chroma-x0.5` / `chroma-x2` | YCbCr | 4:2:0 | none | K.1 | K.2 x 0.5 / x 2 |
  | `xyb-annexk` | XYB | 4:4:4 | none | K.1 (X and Y) | K.2 (B-Y) |
  | `xyb-flat` | XYB | 4:4:4 | none | flat 45 | flat 45 |
  | `xyb-csf32` | XYB | 4:4:4 | none | CSF at 32 px per degree | K.2 |
  | `xyb-b-x0.5` / `-x2` / `-x4` | XYB | 4:4:4 | none | K.1 | K.2 x 0.5 / x 2 / x 4 |
  | `xyb-trellis` | XYB | 4:4:4 | `Trellis` | K.1 | K.2 |

  - **Flat** is 45 everywhere: the geometric mean of K.1.
  - **CSF** steps are inversely proportional to the Mannos-Sakrison (1974) contrast sensitivity
    `A(f) = 2.6 (0.0192 + 0.114 f) exp(-(0.114 f)^1.1)` at each coefficient's radial frequency.
    Coefficient `(u, v)` has frequency `sqrt(u^2 + v^2) / 16` cycles per pixel, times the viewing
    geometry in pixels per degree. Below the CSF's peak the sensitivity is held at its peak value,
    so DC is not quantized hardest. The table is then scaled to K.1's geometric mean.
  - At 32 px per degree (a 96 dpi display at about 48 cm) the CSF table spans 39-75. That is much
    flatter than K.1's 10-121, because the highest DCT frequency is only about 20 cycles per
    degree. At 64 px per degree it is steeper.
  - These tables are experiment inputs, not candidate built-ins. A built-in table would still owe
    the citation `STATUS.md` names.
- **Quality sweep.** q = 30, 35, ..., 95 (14 points) per image and variant.
- **Decode.** libjpeg-turbo, the crate's oracle (`tooling/libjpeg-oracle`), decodes every stream,
  so a defect shared by gamut's encoder and decoder cannot flatter a variant. XYB streams decode to
  their passthrough scaled-XYB samples. The arm inverts these to sRGB through `gamut_color::xyb` and
  the sRGB OETF, rounding to 8 bits, as `tests/xyb.rs` and an ICC-aware consumer do.
- **Metrics,** against the source sRGB:
  - PSNR of BT.601 luma (`0.299 R + 0.587 G + 0.114 B`, unrounded);
  - PSNR over all three channels;
  - SSIM on that luma: Wang et al. 2004, 11x11 Gaussian window with sigma 1.5, K1 = 0.01,
    K2 = 0.03, averaged over the windows that fit;
  - SSIMULACRA 2 on the decoded sRGB (the perceptual metric, issue #972), from
    `tooling/perceptual-oracle`. That dev-only crate wraps `ssimulacra2` 0.5.1, rust-av's pure-Rust
    port of libjxl's reference `tools/ssimulacra2.cc`, and labels the samples sRGB with BT.709
    primaries. 100 is identical and about 90 is visually lossless; lower is worse.

  SSIM enters the rate comparison as `-10 log10(1 - SSIM)`, and SSIMULACRA 2 as its score.

  The first three metrics are mean-squared-error-family: they credit a flat table by construction.
  SSIMULACRA 2 does not. It compares multi-scale structure, ringing and blur in the XYB colour
  space, so it is the column a table conclusion can rest on. It is still one metric over a small
  corpus.
- **Rate comparison.** This is a Bjontegaard-style delta rate:
  - per image, each variant's and the baseline's curves are interpolated piecewise-linearly in
    `ln(bpp)` against the metric;
  - the log-rate gap is averaged over the overlapping metric range (200 points) and reported as
    `exp(mean) - 1`;
  - negative means smaller files at equal fidelity;
  - the mean row is the arithmetic mean over the five images.

  `raw/bdrate-*.csv` holds every per-image value with its overlap range.
- **Corpus.**
  - Four real-content images from libjpeg-turbo's test set (`third_party/libjpeg-turbo/testimages`,
    SHA-256s in `run.json`):
    - `testorig.ppm`, 227x149;
    - `shira_bird8.bmp`, 192x144;
    - `vgl_6434_0018a.bmp`, 119x96;
    - `vgl_6548_0026a.bmp`, 128x95, which is neutral grey: PSNR-Y equals PSNR-RGB on every row, and
      chroma tables do not move it.
  - The decode bench's deterministic photo-like generator at 512x384 (`synthetic-512x384`).
  - The images are small. That is the corpus's main weakness: the repository commits no large
    photographic set this arm could read without a new decoder dependency.
- **`testorig` behaves as a recompressed JPEG.**
  - Its sibling `testorig.jpg` carries the Annex K tables at quality 75: its luma DQT begins
    8, 6, 6, 7, 6, 5, 8, which is K.1 halved.
  - `testorig.ppm` behaves as that file's decode. `annexk` at q75 scores 54.1 dB PSNR-Y, above
    its own 53.5 dB at q95: re-encoding on the grid the image was already quantized to is
    anomalously cheap.
  - This flatters every Annex K-luma variant on that image, so its row is the outlier wherever
    the luma table changes (below).

## Results

Mean rate difference over the five images, in percent. Negative means smaller at equal fidelity.

**YCbCr against `annexk` (4:2:0, Annex K, optimized Huffman tables):**

| variant | PSNR-Y | PSNR-RGB | SSIM-Y | SSIMULACRA 2 |
| --- | ---: | ---: | ---: | ---: |
| `annexk-444` | +22.96 | -2.97 | +25.94 | +4.58 |
| `trellis` (shipped) | -5.19 | -4.47 | -4.04 | -2.95 |
| `trellis-adaptive` (shipped) | -3.01 | -3.60 | -3.57 | -2.63 |
| `luma-flat` | -14.20 | -13.01 | -12.47 | -2.64 |
| `luma-csf32` | -13.50 | -12.24 | -11.81 | -4.06 |
| `luma-csf64` | -4.32 | -4.16 | -4.84 | -3.13 |
| `luma-csf32-trellis` | -20.04 | -20.25 | -17.11 | -8.54 |
| `chroma-x0.5` | +4.77 | -4.97 | +5.79 | -9.72 |
| `chroma-x2` | -4.44 | +9.51 | -4.54 | +16.01 |

**XYB against `xyb-annexk` (the shipped XYB pairing), with the YCbCr baseline for scale:**

| variant | PSNR-Y | PSNR-RGB | SSIM-Y | SSIMULACRA 2 |
| --- | ---: | ---: | ---: | ---: |
| `annexk` (YCbCr 4:2:0) | -38.27 | -18.60 | -39.33 | -4.87 |
| `xyb-flat` | -11.27 | -12.26 | -9.57 | +9.28 |
| `xyb-csf32` | -11.91 | -10.87 | -10.51 | +3.50 |
| `xyb-b-x0.5` | +7.75 | +0.54 | +8.09 | -1.37 |
| `xyb-b-x2` | -4.02 | +5.30 | -4.34 | +9.17 |
| `xyb-b-x4` | -5.73 | +14.83 | -6.24 | +24.55 |
| `xyb-trellis` | -5.94 | -7.38 | -5.71 | -3.89 |

Measured the other way round, against `annexk`, `xyb-annexk` is +68.95% (PSNR-Y), +26.26%
(PSNR-RGB), +73.44% (SSIM-Y) and +15.97% (SSIMULACRA 2). The best XYB variant on PSNR-Y and
SSIM-Y, `xyb-csf32`, is +48.60%, +20.89%, +54.13% and +22.31%; on PSNR-RGB and SSIMULACRA 2
`xyb-trellis` is better, at +17.25% and +10.32%. (The two directions are not negatives of each
other: each is a mean of per-image ratios, over each pair's own overlapping score range.)

**Per image, SSIMULACRA 2, for the variants the verdict turns on** (against `annexk` unless
named):

| variant | `testorig` | `shira_bird8` | `vgl_6434_0018a` | `vgl_6548_0026a` | `synthetic-512x384` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `trellis` | -2.91 | -1.91 | +1.76 | -4.62 | -7.08 |
| `luma-flat` | -13.50 | -0.99 | -7.14 | +2.63 | +5.80 |
| `luma-csf32` | -12.66 | -3.67 | -5.61 | -0.02 | +1.67 |
| `luma-csf32-trellis` | -15.65 | -8.73 | -4.43 | -2.55 | -11.35 |
| `chroma-x0.5` | -13.16 | -4.24 | -22.43 | 0.00 | -8.76 |
| `chroma-x2` | +16.60 | +12.55 | +27.19 | 0.00 | +23.69 |
| `xyb-annexk` | +24.88 | +22.94 | -40.07 | +29.08 | +43.05 |
| `xyb-flat` (vs `xyb-annexk`) | -2.28 | -3.50 | -24.35 | +6.41 | +70.12 |
| `xyb-csf32` (vs `xyb-annexk`) | +1.14 | -7.03 | -5.54 | +0.63 | +28.29 |

On `vgl_6434_0018a`, `annexk`'s SSIMULACRA 2 curve spans only 11.4-32.2: 4:2:0 at q95 still scores
32.2 on that saturated-colour image, where `annexk-444` reaches 71.8 and `xyb-annexk` 80.3. Every
comparison against `annexk` on it is therefore made over a low-quality range, and it is the image
that pulls the XYB mean down. `vgl_6548_0026a` is neutral grey, so chroma tables do not move it
(0.00).

**Per image, PSNR-Y, for the table variants:**

| variant | `testorig` | `shira_bird8` | `vgl_6434_0018a` | `vgl_6548_0026a` | `synthetic-512x384` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `trellis` | -6.86 | -4.02 | -3.93 | -1.35 | -9.78 |
| `luma-flat` | -2.61 | -16.39 | -19.56 | -18.59 | -13.86 |
| `luma-csf32` | -2.84 | -16.35 | -18.84 | -17.10 | -12.37 |
| `luma-csf64` | -6.68 | -6.12 | -2.08 | +0.14 | -6.88 |
| `luma-csf32-trellis` | -6.62 | -21.78 | -23.43 | -18.88 | -29.49 |
| `xyb-flat` (vs `xyb-annexk`) | -6.93 | -26.39 | -16.48 | -15.91 | +9.37 |
| `xyb-csf32` (vs `xyb-annexk`) | +0.58 | -22.55 | -18.28 | -14.72 | -4.59 |

How to read it:

- **Flat and CSF luma tables win on these metrics, for a reason the metrics build in.**
  - `luma-flat` and `luma-csf32` (39-75, nearly flat) are 12-14% better than Annex K on all
    three metrics. Without the recompressed `testorig` they are 16-17% better on PSNR-Y.
  - `luma-csf64`, the steeper CSF, gains only about 4%. The closer a table is to flat, the more
    PSNR and SSIM credit it.
  - That ordering is what an MSE-family metric predicts, and it says nothing about visibility. The
    rate a flat table saves comes from fine low frequencies being coarsened to Annex K's
    high-frequency steps, which a viewer may well see.
  - SSIM-Y agrees with PSNR here: its structure term is still an error correlation on luma and does
    not model frequency-dependent visibility.
  - **SSIMULACRA 2 cuts the gain to 2.6-4.1%** and reorders it: `luma-csf32` (-4.06%) beats
    `luma-flat` (-2.64%), and even the steep `luma-csf64` (-3.13%) beats the flat table. Per image
    the flat table loses on two inputs, the neutral-grey `vgl_6548_0026a` and the synthetic one.
    The largest gain is on the recompressed `testorig` (-13.50%), the image where PSNR-Y showed
    almost none. One untested explanation is that a table off Annex K's grid avoids
    requantization artefacts this metric weighs and PSNR does not.
- **The trellis composes with the tables.** `luma-csf32-trellis` (-20.04%) is close to the sum of
  `luma-csf32` (-13.50%) and `trellis` (-5.19%). On SSIMULACRA 2 it is more than the sum (-8.54%
  against -4.06% and -2.95%). The trellis normalizes distortion by each coefficient's step (`rd`
  module docs), so the table it is handed is still its perceptual model, and a different table
  moves both.
- **The shipped trellis modes, measured on this corpus.** `trellis` saves 4-5% and
  `trellis-adaptive` 3-4% at equal PSNR/SSIM, and 2.95% and 2.63% at equal SSIMULACRA 2. #333's
  battery measured 8.7% at a fixed quality, which is a different comparison. `trellis-adaptive`
  trades some PSNR for spending more bits on flat blocks; SSIMULACRA 2 does not reward that bet
  on this corpus either.
- **Chroma balance.** On the MSE-family metrics each change moves rate between the luma metrics
  and RGB PSNR, and neither direction wins on all three. 4:4:4 is the same trade made by the
  sampling instead of the table: +23% luma rate for -3% RGB-PSNR rate. On the saturated-colour
  `vgl_6434_0018a` it is -30% RGB.
  - **SSIMULACRA 2 takes a side.** Finer chroma (`chroma-x0.5`) gains on every colour image
    (-4.2% to -22.4%) and coarser chroma (`chroma-x2`) loses on every one (+12.6% to +27.2%). The
    metric weighs colour error that luma PSNR and SSIM do not see at all.
  - 4:4:4 costs 4.58% on the mean, so on this corpus finer chroma steps are a cheaper route to
    colour fidelity than full-resolution chroma. Its worst image is `vgl_6434_0018a` (+17.77%),
    but only over the 11.4-32.2 range 4:2:0 reaches there: above it 4:2:0 has no point to compare,
    and 4:4:4 goes on to 71.8.
- **XYB under sRGB metrics.**
  - **The 8-bit sample floor.** At q95, `xyb-annexk` reaches 40.9-50.7 dB PSNR-Y where `annexk`
    reaches 42.1-53.5. Quantizing the scaled-XYB samples to 8 bits caps fidelity before any DCT
    quantization: this is the roughly 40 dB floor `tests/xyb.rs` gates at.
  - **The metrics.** PSNR and SSIM in sRGB measure what YCbCr's error structure is easiest on.
    XYB's design target, a perceptual distance, is not among them.
  - So these numbers do not show that XYB is worse to look at. They show only that it is worse
    for PSNR and SSIM in sRGB.
  - **SSIMULACRA 2 narrows the gap** from +69% to +16% on the mean. Per image, XYB still costs
    23-43% on four inputs and gains 40% on `vgl_6434_0018a`, the image where 4:2:0 caps at a
    score of 32. Against `annexk-444`, which shares XYB's full-resolution chroma, the shipped
    XYB pairing costs more still: `annexk-444` is -11.38% against `xyb-annexk`.
  - **Within XYB.** On PSNR and SSIM, flattening X/Y (`xyb-flat`, `xyb-csf32`) is the largest
    gain, on the same MSE logic as YCbCr. A coarser B table (`xyb-b-x2`, `-x4`) saves luma rate
    and costs colour. A finer one (`xyb-b-x0.5`) loses on every MSE-family metric. On SSIMULACRA 2
    the B table behaves like YCbCr chroma: the finer one gains 1.37% and the coarser ones cost
    9.17% and 24.55%. The flat and CSF X/Y tables cost 9.28% and 3.50% on the mean, a mean the
    synthetic image carries (+70.12%, +28.29%).
  - **The synthetic image.** It reverses `xyb-flat` on PSNR-Y (+9.37%) and dominates it on
    SSIMULACRA 2. It is the only input whose noise is drawn independently per channel. One
    untested explanation is that a flat X/Y table spends its finer high-frequency steps on that
    noise.

## What is left

- **A second perceptual metric, and the reference.** SSIMULACRA 2 is now the deciding column
  (#972), but it is one metric, and it comes from rust-av's port rather than libjxl's
  `tools/ssimulacra2.cc`. The port's own test holds it within 0.25 of the reference's score on one
  fixture pair; this record does not re-check that. Butteraugli, or the reference SSIMULACRA 2,
  would need libjxl's internal C++ API, which the workspace's one libjxl build (`gamut-jxl-sys`)
  does not publish (`tooling/perceptual-oracle/src/lib.rs`).
- **A separate X table in XYB mode.** The encoder quantizes X and Y with one table (destination 0)
  and B-Y with the other, and `QuantTables` carries exactly two. Measuring a separate X table
  needs an encoder surface change, not an experiment arm.
- **DC trellis and overshoot deringing.**
  - The trellis's DC term goes through the §F.1.2.1 predictor inside the encoder.
  - Deringing (mozjpeg's overshoot) needs pre-FDCT samples above 255, which `Rgb8` input cannot
    carry.
  - Both need a variant that patches `crates/gamut-jpeg/src` for one build. The #514 / #612
    records do this with a `variant.patch`; this record keeps to the public API.
- **A larger photographic corpus.** The five inputs here are small, and one is a recompressed
  JPEG. The finer chroma table, the one candidate default this record turns up, should be
  measured on one before anything ships.

## Reproducing

`mise run experiment 29` regenerates every file in `raw/` and `run.json`. It needs the
`third_party/libjpeg-turbo` submodule, for the oracle build and the test images, and a C toolchain.
Its stdout prints the two mean tables above. Encodes are deterministic, so a re-run on the same
`git_sha` reproduces `raw/` exactly. The XYB rows hold within gamut-color's `f64` Tier-1
determinism, so they may differ in the last bits across platforms, and the SSIMULACRA 2 column is
computed in `f32` by the metric crate, with the same caveat. The re-run that added that column
(at `git_sha` in `run.json`) reproduced every earlier PSNR, SSIM and BD-rate value byte for byte.
`EXPERIMENT_SYNTHETIC` (default
`512x384`) sets the synthetic image's size.
