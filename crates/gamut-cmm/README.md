# gamut-cmm

`gamut-cmm` is the workspace's **ICC colour management module** (CMM): it builds runnable colour
transforms from parsed ICC profiles and applies them to pixels.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to be the transform *engine*
over [`gamut-icc`](../gamut-icc)'s parsed profiles:

- **Memory-safe.** `#![forbid(unsafe_code)]`; pure scalar `f64` math over borrowed sample
  buffers, no I/O.
- **Clean-slate from the spec, behaviour pinned to the reference CMM.** Data layouts follow
  **ICC.1:2022** ([`references/icc`](../../references/icc)); where ICC.1 is silent on CMM
  behaviour (interpolation, clamping), observable semantics follow **Little-CMS** (lcms2), the
  differential oracle ([`references/cmm`](../../references/cmm/README.md)).
- **Layered on shared crates.** Profiles are parsed by [`gamut-icc`](../gamut-icc); colorimetric
  primitives come from [`gamut-color`](../gamut-color); this crate owns only linking and
  evaluation.

## Use cases

- **Colour-correct decoding** — convert decoded pixels through an embedded profile (a PNG
  `iCCP`, a JPEG `APP2`, a TIFF/DNG tag 34675) into a display or working space, straight over
  `gamut_core::PixelFormat` buffers (`transform_interleaved_u8` and friends).
- **Profile-accurate encoding** — bring working-space pixels into the space an embedded profile
  describes before encoding.
- **Prepress workflows** — chain profiles through abstract edits (`IccTransform::chain`), apply
  finished device links (`IccTransform::device_link`), soft-proof a printer on a display
  (`IccTransform::proofing`), and flag out-of-gamut colours (`GamutCheck`).
- **Custom pipelines** — assemble a validated stage chain by hand and run it through the same
  `Transform` entry point a linked profile pair will use.

## Integration with other gamut libraries

`gamut-cmm` sits between `gamut-icc` and pixel data: a format crate extracts the profile blob,
`gamut-icc` parses it, and this crate turns it into a runnable [`Transform`]. Samples are
interleaved `f64` — device channels encoded in `[0, 1]`, PCS seams decoded colorimetry (XYZ with
D50 `Y = 1.0`, Lab with `L*` in `0..=100`). Evaluation is Tier-1 (correctness only, not
bit-reproducible), the same posture as `gamut-color`.

## Usage

```rust
use gamut_cmm::{Pipeline, Stage, Transform};

// A toy transform: scale-and-offset each RGB channel, then clamp to [0, 1].
let scale = Stage::Matrix {
    m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
    offset: [0.25, 0.25, 0.25],
};
let pipeline = Pipeline::new(3, 3, vec![scale, Stage::Clamp { channels: 3 }])?;

let src = [1.0, 0.5, 2.0]; // one interleaved RGB pixel
let mut dst = [0.0; 3];
pipeline.transform(&src, &mut dst)?;
assert_eq!(dst, [0.75, 0.5, 1.0]);
# Ok::<(), gamut_cmm::CmmError>(())
```

## Status

The architectural keystone (epic #323, scaffold #324): the `Pipeline`/`Stage` evaluation model
with construction-time channel validation, the object-safe `Transform` entry trait, and the
typed `CmmError`. Stages cover identity, clamp, the 3×3 affine matrix and its rectangular
`MatrixN` sibling, per-channel tone curves (#325) — `ToneCurve` evaluates and inverts
`curveType`/`parametricCurveType` elements (analytic closed forms plus an lcms2-shaped numeric
reversal) behind `Stage::Curves` — and multi-dimensional CLUTs (#326): `ClutTable`
interpolates 1–15-input grids behind `Stage::Clut`, with lcms2's exact tetrahedral
decomposition (default from 3 inputs, recursing lcms2's slice-and-blend above 3-D) and
selectable N-D multilinear (`ClutInterpolation`, the hook #328 uses for Lab-indexed CLUTs).
Profile linking (#327, #328): `link::{device_to_pcs, pcs_to_device}` build runnable
pipelines from **matrix/TRC ("shaper") profiles** — RGB (XYZ and Lab PCS, via the
`XyzToLab`/`LabToXyz` bridge stages) and gray, v2 and v4, both directions, with the settled
chromatic-adaptation convention (colorants as-is, `chad` unread on the relative path) — and
from **LUT profiles**: `lut8`/`lut16`/`lutAToB`/`lutBToA` tags (CMYK printers, camera input
profiles, device links), selected per rendering intent with lcms2's intent→tag tables and
perceptual fallback, the lut16 v2-Lab encoding rule, and trilinear interpolation for
Lab-indexed CLUTs. Rendering intents and black-point compensation (#329): `IccTransform::between(&src, &dst,
TransformOptions { intent, black_point_compensation })` links a pair end to end, applying
the ICC-absolute media-white scaling (`diag(wIn/wOut)` at the XYZ seam, with lcms2's
v2-display and missing-`wtpt` quirks) or black-point compensation (the ISO 18619/WP40
linear XYZ scaling over `bpc::{detect_black_point, detect_destination_black_point}` —
lcms2's estimators transcribed, `bkpt` deliberately ignored, BPC forced for v4 destinations
under perceptual/saturation exactly as lcms2 does). **v1 is complete** with transform
chaining and typed pixel buffers (#330): `IccTransform::chain` links any number of profiles
(device-link and abstract classes included) through lcms2's `DefaultICCintents` algorithm,
`IccTransform::device_link` applies a finished link profile, `IccTransform::proofing`
builds the four-profile soft-proofing chain, `GamutCheck` implements lcms2's
double-round-trip gamut test, and the `image` module applies any `Transform` to interleaved
or planar `u8`/`u16` buffers tagged with `gamut_core::PixelFormat` (alpha passthrough,
round-half-up re-encoding). The whole engine is held to the epic's **conformance gate**: a
profile battery × 4 intents × BPC on/off differenced against Little-CMS with per-class
max-ΔE₀₀ bounds (see STATUS.md's threshold table) — all documented in
[STATUS.md](STATUS.md).

## Deferred

iccMAX (`ICC.2`), `multiProcessElementsType` (`mpet`), and the `DToBx`/`BToDx` tags are out of
scope — the still-image profiles embedded in real images are all ICC.1 v2/v4, and the lcms2
oracle does not implement iccMAX. Pipeline optimization (lcms2's stage collapsing/CLUT
resampling) is deferred to issue #372; per-hop intent arrays for chains stay internal until a
consumer needs them. See [STATUS.md](STATUS.md).

## License

Licensed under either of MIT or Apache-2.0 at your option.
