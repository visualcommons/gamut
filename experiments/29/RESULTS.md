# Experiment #29: quantization choices gamut-jpeg's encoder does not make yet

**Question.** #29 asks for clean-slate experiments with JPEG encoder quality, kept as a record of
what was tried. This first record measures the quantization choices the shipped encoder does not
make, all reachable through its public API:

- alternate luminance base tables (flat; inverse contrast-sensitivity);
- a different luma/chroma balance;
- tables aimed at the XYB colour mode, whose Annex K pairing `STATUS.md` calls a placeholder.

The shipped Annex K path and the shipped trellis modes are the yardsticks.

**Verdict: nothing here justifies changing a shipped default yet. The deciding gap is the metric,
not the encoder.**

- **Flat and CSF luma tables.** On PSNR and SSIM they beat Annex K by 12-14% at equal fidelity, and
  they stack with the trellis to about 20%. But PSNR and SSIM are mean-squared-error-family
  metrics, and a flat table is the MSE-optimal shape for an orthonormal transform. These metrics
  cannot credit what Annex K is for: coarse high frequencies where the eye does not see the error.
  The repository has no perceptual-metric oracle (butteraugli, SSIMULACRA 2), so these numbers
  cannot show that a flatter table is better. They show only that it is better for PSNR.
- **Luma/chroma balance.** It is a trade, not a gain. Halving the chroma steps costs 5% on the luma
  metrics and saves 5% on RGB PSNR. Doubling them does the reverse, with a 10% RGB-PSNR cost.
- **XYB.** On every table tried, XYB costs far more rate than YCbCr 4:2:0 at equal sRGB PSNR or
  SSIM: +69% with the shipped pairing, and about +49% with the best table here. Flat or CSF tables
  on X/Y are the clearest within-XYB gain (11-12%). Coarsening the B table helps the luma metrics
  and hurts colour, as chroma does in YCbCr. One limit is the encoder's own surface: X and Y share
  quantization destination 0, so the obvious next experiment, a coarser X than Y, cannot be
  expressed (see *What is left*).
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
    K2 = 0.03, averaged over the windows that fit.

  SSIM enters the rate comparison as `-10 log10(1 - SSIM)`.
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

| variant | PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | ---: | ---: | ---: |
| `annexk-444` | +22.96 | -2.97 | +25.94 |
| `trellis` (shipped) | -5.19 | -4.47 | -4.04 |
| `trellis-adaptive` (shipped) | -3.01 | -3.60 | -3.57 |
| `luma-flat` | -14.20 | -13.01 | -12.47 |
| `luma-csf32` | -13.50 | -12.24 | -11.81 |
| `luma-csf64` | -4.32 | -4.16 | -4.84 |
| `luma-csf32-trellis` | -20.04 | -20.25 | -17.11 |
| `chroma-x0.5` | +4.77 | -4.97 | +5.79 |
| `chroma-x2` | -4.44 | +9.51 | -4.54 |

**XYB against `xyb-annexk` (the shipped XYB pairing), with the YCbCr baseline for scale:**

| variant | PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | ---: | ---: | ---: |
| `annexk` (YCbCr 4:2:0) | -38.27 | -18.60 | -39.33 |
| `xyb-flat` | -11.27 | -12.26 | -9.57 |
| `xyb-csf32` | -11.91 | -10.87 | -10.51 |
| `xyb-b-x0.5` | +7.75 | +0.54 | +8.09 |
| `xyb-b-x2` | -4.02 | +5.30 | -4.34 |
| `xyb-b-x4` | -5.73 | +14.83 | -6.24 |
| `xyb-trellis` | -5.94 | -7.38 | -5.71 |

Measured the other way round, against `annexk`, `xyb-annexk` is +68.95% (PSNR-Y), +26.26%
(PSNR-RGB) and +73.44% (SSIM-Y). The best XYB variant on PSNR-Y and SSIM-Y, `xyb-csf32`, is
+48.60%, +20.89% and +54.13%; on PSNR-RGB `xyb-trellis` is better, at +17.25%. (The two directions are not negatives of each other: each is a mean of per-image ratios.)

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
- **The trellis composes with the tables.** `luma-csf32-trellis` (-20.04%) is close to the sum of
  `luma-csf32` (-13.50%) and `trellis` (-5.19%). The trellis normalizes distortion by each
  coefficient's step (`rd` module docs), so the table it is handed is still its perceptual model,
  and a different table moves both.
- **The shipped trellis modes, measured on this corpus.** `trellis` saves 4-5% and
  `trellis-adaptive` 3-4% at equal PSNR/SSIM. #333's battery measured 8.7% at a fixed quality,
  which is a different comparison. `trellis-adaptive` trades some PSNR for spending more bits on
  flat blocks; that is a perceptual bet these metrics cannot reward.
- **Chroma balance.** Each change moves rate between the luma metrics and RGB PSNR, and neither
  direction wins on all three. 4:4:4 is the same trade made by the sampling instead of the table:
  +23% luma rate for -3% RGB-PSNR rate. On the saturated-colour `vgl_6434_0018a` it is -30% RGB.
- **XYB under sRGB metrics.**
  - **The 8-bit sample floor.** At q95, `xyb-annexk` reaches 40.9-50.7 dB PSNR-Y where `annexk`
    reaches 42.1-53.5. Quantizing the scaled-XYB samples to 8 bits caps fidelity before any DCT
    quantization: this is the roughly 40 dB floor `tests/xyb.rs` gates at.
  - **The metrics.** PSNR and SSIM in sRGB measure what YCbCr's error structure is easiest on.
    XYB's design target, a perceptual distance, is not among them.
  - So these numbers do not show that XYB is worse to look at. They show only that it is worse
    for PSNR and SSIM in sRGB.
  - **Within XYB.** Flattening X/Y (`xyb-flat`, `xyb-csf32`) is the largest gain, on the same MSE
    logic as YCbCr. A coarser B table (`xyb-b-x2`, `-x4`) saves luma rate and costs colour. A
    finer one (`xyb-b-x0.5`) loses on every metric, so the shipped K.2 for B-Y is not too coarse.
  - **The synthetic image.** It reverses `xyb-flat` (+9.37% PSNR-Y). It is the only input whose
    noise is drawn independently per channel. One untested explanation is that a flat X/Y table
    spends its finer high-frequency steps on that noise.

## What is left

- **A perceptual metric.** Every table conclusion above waits on one. The workspace has no
  butteraugli or SSIMULACRA 2 implementation or oracle, and `gamut-jpeg` cannot take one as a
  dev-dependency without a new external crate (the release-deps rule rules out a sibling crate).
  Until one exists, a table chosen on this record would be chosen for PSNR.
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
  JPEG.

## Reproducing

`mise run experiment 29` regenerates every file in `raw/` and `run.json`. It needs the
`third_party/libjpeg-turbo` submodule, for the oracle build and the test images, and a C toolchain.
Its stdout prints the two mean tables above. Encodes are deterministic, so a re-run on the same
`git_sha` reproduces `raw/` exactly. The XYB rows hold within gamut-color's `f64` Tier-1
determinism, so they may differ in the last bits across platforms. `EXPERIMENT_SYNTHETIC` (default
`512x384`) sets the synthetic image's size.
