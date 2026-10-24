# Experiment #974: DC trellis and overshoot deringing in gamut-jpeg

**Question.** Experiment #29 kept to `JpegEncoder`'s public API, so it could not measure two
techniques that need the encoder's internals (`experiments/29/RESULTS.md`, *What is left*):

- **DC trellis.** The shipped trellis (`rd` module) leaves DC at plain rounding. A DC trellis chooses
  each block's DC jointly with its neighbours', because DC is coded as a difference against the
  previous block's (§F.1.2.1).
- **Overshoot deringing** (mozjpeg). A decoder clamps reconstructed samples to 255, so samples at
  the ceiling may be coded as any value at or above it. Raising them past 255 before the FDCT can
  avoid the ringing a hard edge into white costs. `Rgb8` input cannot carry samples above 255, so
  this has to happen inside the encoder.

Each technique is a patch of `crates/gamut-jpeg/src`, applied for one build of #29's measurement
arm and reverted afterwards, as the #514 and #612 records do. Nothing under `crates/gamut-jpeg/src`
changes on master.

**Verdict: neither technique justifies shipping on this evidence. The DC trellis is negligible.
Deringing is a large, narrow win that these metrics can only partly judge.**

- **DC trellis: nothing to ship.** On the four RD variants it moves rate by -0.05% to +0.04% at
  equal fidelity, averaged over the seven images. No single image moves more than 0.24%. It
  changes stream sizes on about 70% of the RD quality points, so it does act; the DC bits it
  trades are simply too few to matter. The `rd` module's deferral of DC trellis "for marginal gain" stands, now measured.
- **Overshoot deringing: -13% to -16% on black text on white, about zero elsewhere.** On
  `text-384x256` every variant saves 12-16% rate at equal PSNR, and 24-25% at equal SSIM. On the
  other six images it moves rate by under 0.5% either way, with one outlier (below). The mean of
  about -2% (PSNR) and -3.5% (SSIM) over seven images is that one image divided by seven, not a
  general gain.
  - The win is real on the metrics measured here: PSNR is taken after libjpeg-turbo's decode
    clamps to 255, so the overshoot is credited exactly as a viewer would see it.
  - What the metrics cannot settle: whether the curve deringing paints through the ceiling moves
    visible error to the edges of the saturated runs. That needs the perceptual-metric oracle #29
    names, as do the table conclusions there.
  - A shipped version would be an encoder free choice (T.81 leaves the samples fed to the FDCT to
    the encoder), but it would need its own oracle evidence first, and an integer formulation:
    the patch uses mozjpeg's `f32` curve, and the crate's encoder is integer-deterministic.

## Results

Mean rate difference of each patched build against the same variant in the baseline build, over
the seven images, in percent. Negative means smaller at equal fidelity. `raw/bdrate-*.csv` holds
every per-image value with its overlap range.

**`dc-trellis`.** The thirteen variants without RD optimization are byte-identical to the baseline
build (`raw/changed.csv`: 0 of 98 points changed for each), which confirms the patch is confined
to the trellis path. The RD variants:

| variant | PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | ---: | ---: | ---: |
| `trellis` | -0.01 | -0.00 | +0.04 |
| `trellis-adaptive` | -0.01 | -0.01 | +0.04 |
| `luma-csf32-trellis` | -0.01 | -0.03 | -0.01 |
| `xyb-trellis` | -0.05 | -0.04 | -0.02 |

The largest single-image value is `xyb-trellis` on `synthetic-512x384`, -0.24% PSNR-Y.

**`overshoot-deringing`, mean over the seven images:**

| variant | PSNR-Y | PSNR-RGB | SSIM-Y |
| --- | ---: | ---: | ---: |
| `annexk` | -1.91 | -1.93 | -3.42 |
| `annexk-444` | -1.94 | -1.90 | -3.42 |
| `trellis` | -1.87 | -1.91 | -3.49 |
| `trellis-adaptive` | -1.79 | -1.81 | -3.35 |
| `luma-flat` | -1.84 | -1.85 | -3.42 |
| `luma-csf32` | -1.87 | -1.88 | -3.52 |
| `luma-csf64` | -2.13 | -2.16 | -3.75 |
| `luma-csf32-trellis` | -1.91 | -1.93 | -3.61 |
| `chroma-x0.5` | -1.30 | -1.94 | -3.43 |
| `chroma-x2` | -1.91 | -1.93 | -3.42 |
| `xyb-annexk` | -2.30 | -2.35 | -3.52 |
| `xyb-flat` | -2.28 | -2.18 | -3.54 |
| `xyb-csf32` | -2.25 | -2.32 | -3.57 |
| `xyb-b-x0.5` | -2.31 | -2.36 | -3.52 |
| `xyb-b-x2` | -2.30 | -2.30 | -3.52 |
| `xyb-b-x4` | -2.31 | -2.25 | -3.53 |
| `xyb-trellis` | -2.29 | -2.32 | -3.65 |

**`overshoot-deringing`, per image, PSNR-Y, for a few variants:**

| variant | `testorig` | `shira_bird8` | `vgl_6434_0018a` | `vgl_6548_0026a` | `synthetic-512x384` | `clipped-512x384` | `text-384x256` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `annexk` | +0.04 | +0.02 | -0.03 | -0.19 | +0.05 | +0.03 | -13.30 |
| `trellis` | -0.01 | +0.00 | +0.03 | -0.36 | +0.04 | +0.04 | -12.84 |
| `luma-flat` | +0.03 | +0.00 | +0.07 | +0.42 | +0.16 | +0.21 | -13.76 |
| `chroma-x0.5` | +4.37 | +0.01 | -0.05 | -0.19 | +0.03 | +0.01 | -13.30 |
| `xyb-annexk` | -0.02 | +0.02 | -0.24 | -0.37 | -0.01 | -0.18 | -15.29 |

How to read it:

- **Deringing acts almost everywhere, and matters almost nowhere.** It changed stream sizes on 83-92
  of each variant's 98 points (`raw/changed.csv`), because nearly every image has some block that
  touches 255. But apart from one outlier (below), only on `text-384x256` does it change rate by
  more than half a percent.
- **Why text and not `clipped-512x384`.** The clipped photograph has 6.7% white pixels
  (`raw/inputs.csv`), but they sit in smooth highlights: a block partly at 255 is mostly a gentle
  ramp into the ceiling, which costs few AC bits to begin with. The text image (82% white) is all
  hard edges between ink and white, the ringing case deringing exists for.
- **XYB gains a little more** (-15% on text against -13% for YCbCr). The patch changed XYB
  streams, so some scaled-XYB plane of the white background reaches 255; which plane, and why the
  gain is larger, was not measured.
- **The outlier, `chroma-x0.5` on `testorig`: +4.37% PSNR-Y.** `testorig` is a recompressed JPEG
  (#29), so its rate-distortion curve is irregular. The other two metrics for that cell are
  +0.03% (PSNR-RGB) and +0.05% (SSIM-Y) in `raw/bdrate-overshoot-deringing.csv`, so a curve
  irregularity is the likelier reading than a real regression. That is an untested explanation.
- **DC trellis.** It changes 67-83 of the 98 points per RD variant, and never by enough to move
  the BD-rate a tenth of a percent on average. DC is one symbol per block against up to 63 AC
  symbols. With a step-normalized distortion and the AC trellis's λ, the only DC moves it buys
  are near-ties.

## Method

- **Builds.** Three builds of #29's arm (`crates/gamut-jpeg/examples/experiment_29.rs`): the tree
  as checked out (`baseline`), then the same tree with each patch applied. Each build runs #29's
  whole `measure` sweep, so every one of its 17 encoder configurations ("variants": tables,
  subsampling, RD mode, colour mode; see `experiments/29/RESULTS.md`) is measured in every build.
  A patched variant is compared with **the same variant in the baseline build**, so the only
  difference is the patch.
- **`dc-trellis.patch`** (`rd.rs`, `encoder.rs`). With RD optimization on (`Trellis`,
  `TrellisAdaptive`), `encode_scan` first walks the scan in coding order and collects every
  block's unquantized DC per component, marking where a restart resets the predictor. A Viterbi
  pass (`rd::dc_trellis`) then picks each block's quantized DC from two candidates, the nearest
  multiple of the step and the other neighbouring one. It minimizes the sum of each block's
  distortion and λ times the bits of its DC difference. The cost model is the AC trellis's own:
  - distortion normalized by the step;
  - the same λ, unmodulated, so `TrellisAdaptive` uses the plain λ for DC;
  - rates from the typical Annex K.3/K.4 DC table (code length plus `SSSS` magnitude bits),
    whatever table the stream later codes with.

  With λ = 0 the choice is plain rounding. The AC trellis is unchanged. The patch covers the
  baseline process only; the progressive encoder keeps plain DC. #29's arm encodes baseline only,
  so this does not affect the measurement, but a shipped version would have to do the same in
  `progressive.rs` to keep the crate's progressive-equals-baseline coefficient invariant.
- **`overshoot-deringing.patch`** (`encoder.rs`). mozjpeg's `preprocess_deringing`, applied to
  every level-shifted block before the FDCT, in every component, both processes and every
  variant. In a block that has some but not all samples at 255:
  - each run of 255s in zig-zag order is replaced by a Catmull-Rom curve through the ceiling;
  - the curve's end slopes come from the samples on either side of the run;
  - values are capped at 255 + min(31, 2 × the DC step, the block's mean headroom below 255 per
    saturated sample).

  The algorithm follows mozjpeg's `jcdctmgr.c`. mozjpeg is not vendored here, so the patch is
  written from its published algorithm, not diffed against it.
- **Corpus.** #29's five inputs (four libjpeg-turbo test images and the 512x384 synthetic), plus
  two inputs that `crates/gamut-jpeg/examples/experiment_974.rs` generates. Deringing only acts on
  saturated content, and #29's corpus carries little of it:
  - `clipped-512x384`: the synthetic with every channel mapped through `2 (v - 64)` and clamped, so
    its highlights clip to white like an over-exposed photograph;
  - `text-384x256`: dark one- and two-pixel strokes in glyph-sized boxes on white, the
    black-text-on-white case deringing targets.

  `raw/inputs.csv` gives each generated image's share of pure-white pixels.
- **Decode, metrics, rate comparison.** As #29: libjpeg-turbo (the crate's oracle) decodes every
  stream. The metrics are PSNR on BT.601 luma, PSNR over RGB, and SSIM on luma. Rates are compared
  by a Bjontegaard-style delta over q = 30..95. Every value comes from #29's `bdrate` subcommand,
  run once per variant on that variant's rows from the baseline build and the patched build.
  Negative means smaller files at equal fidelity.
- **Which points a patch touched.** `raw/changed.csv` counts, per patch, variant and image, the
  quality points whose stream size differs from the baseline build's. It shows where each patch
  was a no-op: a deringing build codes a block with no saturated sample exactly as the baseline
  does, and the DC trellis does nothing without RD optimization.

## What is left

- **A perceptual metric.** Whether deringing's text gain survives a perceptual comparison, and
  whether it moves visible error to the edges of saturated runs, waits on the same
  perceptual-metric oracle #29's table conclusions wait on.
- **More saturated content.** The text image is synthetic, with one ink level and no
  anti-aliasing. Scanned documents, screenshots and anti-aliased text would show whether the gain
  holds on real saturated content.
- **A DC trellis with a different cost model** (more candidates, a DC-specific λ, or rates from the
  optimized tables) is not ruled out by this record, but the measured ceiling of the two-candidate
  version gives it little room.

## Reproducing

`mise run experiment 974` regenerates every file in `raw/` and `run.json`. It needs the
`third_party/libjpeg-turbo` submodule, for the oracle build and the test images, and a C toolchain.
It refuses to run while `crates/gamut-jpeg/src` has uncommitted changes, applies each patch for one
build, and reverts it on any exit. Its stdout prints the mean tables above and the per-variant
counts of changed points. Encodes are deterministic, so a re-run on the same `git_sha` reproduces
`raw/` exactly, within gamut-color's `f64` Tier-1 determinism for the XYB rows.
`EXPERIMENT_SYNTHETIC` (default `512x384`) sets the synthetic image's size.
