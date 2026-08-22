# ICC colour management module — CMM behaviour references (issue #323)

Reference material for the `gamut-cmm` crate — the colour management module that builds and
applies colour transforms from the ICC profiles parsed by `gamut-icc`.

The profile *format* is specified by **ICC.1:2022**, vendored under
[`references/icc/`](../icc/README.md) together with the legacy v2 edition; the numeric encodings
this CMM consumes (fixed-point formats, PCS Lab/XYZ encodings) are documented there and in
[`references/color/`](../color/README.md). The format specification, however, pins down **data
layouts, not CMM behaviour**: interpolation methods, clamping, evaluation precision, and much of
intent handling are explicitly left to the CMM implementation.

## Behavioural oracle — Little-CMS

`gamut-cmm`'s behavioural oracle is therefore **Little-CMS (lcms2)**, the de-facto reference
CMM: differential tests link it dev-only through `tooling/lcms2-oracle`, built from the
`third_party/lcms2` submodule (**lcms2 2.19**). Where observable behaviour is unspecified by
ICC.1 — e.g. clamp semantics for NaN and out-of-range samples, CLUT interpolation — `gamut-cmm`
matches lcms2 and documents the choice at the API (see `crates/gamut-cmm`).

## Conformance

Differential oracle against **Little-CMS (lcms2)** (C FFI, `tooling/lcms2-oracle`) for
transform behaviour, culminating in the epic's **conformance gate**
(`crates/gamut-cmm/tests/oracle_conformance.rs`): a synthesized profile battery — matrix/TRC
shapers (sRGB, Display P3, wide-gamut γ2.2, gray) and LUT profiles (`scnr` mAB, CMYK `prtr` in
v4/mAB and v2/lut16 serializations) — paired across shaper↔shaper, gray↔shaper, shaper↔LUT and
LUT↔LUT under **all four intents × BPC on/off**, with per-scenario-class max-ΔE₀₀ bounds
against two oracle configurations: lcms2's full-precision float pipelines
(`TYPE_*_DBL`, `NOOPTIMIZE|NOCACHE`) and its default optimized 16-bit path (`TYPE_*_16`). The
same gate covers multi-profile chains vs `cmsCreateMultiprofileTransform`, device-link
transforms vs the one-profile `cmsCreateTransform`, soft proofing vs
`cmsCreateProofingTransform(SOFTPROOFING)`, and gamut-check classification vs
`cmsFLAGS_GAMUTCHECK` alarm substitution. Measured maxima and asserted thresholds are
tabulated with justifications in [`gamut-cmm/STATUS.md`](../../crates/gamut-cmm/STATUS.md);
every profile is synthesized in memory and both sides read the same serialized bytes, so the
comparisons isolate evaluation semantics from tag quantization.

## Vendored primary sources

| file | source |
|------|--------|
| `WP40-Black_Point_Compensation_2010-07-27.pdf` | ICC white paper 40, *Black Point Compensation* — color.org. Describes the linear-XYZ-scaling BPC method (the same method ISO 18619:2015 later standardized; see below) and its rationale. |
| `adobebpc.pdf` | Adobe Systems, *Adobe Systems' Implementation of Black Point Compensation* — originally https://www.color.org/adobebpc.pdf, whose live URL now returns 404; recovered from the Internet Archive snapshot of 2026-02-10 of that URL. The primary source for the destination-black round-trip ramp estimator (`cmsDetectDestinationBlackPoint` cites it as "the Adobe paper"). |
| `BlackPointCompensationTests.pdf` | *Black point compensation tests* — littlecms.com. Little-CMS's own BPC conformance notes. |
| `render.pdf` | ICC, *Rendering intents* overview — color.org. The four ICC rendering intents and where CMM-side math applies (absolute white rescaling) versus profile-baked renderings. |
| `ICCSpecRevision_22_02_05_PRMG.pdf` | ICC specification revision note on the Perceptual Reference Medium Gamut — color.org. Grounds the v4 position that perceptual tables target the PRM/PRMG, so a CMM applies no additional gamut mapping of its own. |

## Not vendored (paywalled — constants transcribed inline by the PRs that need them)

- **ISO 18619:2015** (Image technology colour management — black point compensation) — ISO,
  paywalled. The BPC algorithm implemented in #329; per the ICC, white paper WP40 (vendored
  above) describes the **same** linear XYZ scaling method, so WP40 + the lcms2 transcription
  below stand in for the paywalled text.
- **ISO 12640-3** (Graphic technology — prepress digital data exchange — Part 3: CIELAB standard
  colour image data, SCID) — ISO, paywalled.
- **Kasson, Nin, Plouffe & Hafner, "Performing color space conversions with three-dimensional
  linear interpolation", *J. Electronic Imaging* 4(3), 1995** — paywalled; the tetrahedral
  interpolation primary source for #326, transcribed below in the concrete form lcms2
  implements (`TetrahedralInterpFloat`, credited in-source to "Sakamoto's algorithm").

## Transcription: tetrahedral CLUT interpolation (#326)

From `third_party/lcms2` (lcms2 2.19), `src/cmsintrp.c:620-724` (`TetrahedralInterpFloat`);
the paywalled origin of the decomposition is Kasson–Nin–Plouffe–Hafner 1995 (above).
Implemented by `gamut-cmm`'s `ClutTable` (`crates/gamut-cmm/src/clut.rs`).

**Cell mapping** (`cmsintrp.c:223-227, 638-655`). Per input channel: `fclamp` maps NaN and
every value below `1e-9` (all negatives included) to `0.0` and everything above `1.0` to
`1.0`; then `px = fclamp(in) · Domain` with `Domain = gridPoints − 1`, lower node `x0 = ⌊px⌋`,
fraction `rx = px − x0`, and the upper node equals the lower **when `fclamp(in) >= 1.0`**
(otherwise `x0 + 1`) — the edge rule that keeps the top grid plane in bounds.

**Decomposition.** The unit cube splits into six tetrahedra selected by ordering the three
fractions; the interpolant is `c0 + c1·rx + c2·ry + c3·rz` with `c0 = d(X0,Y0,Z0)` and the
corner differences below, where `d(·)` are the cell's corner samples per output channel.
The branches are tested with `>=` **in exactly this order** (ties are order-dependent):

| # | condition | `c1` | `c2` | `c3` |
|---|-----------|------|------|------|
| 1 | `rx ≥ ry && ry ≥ rz` | `d(X1,Y0,Z0) − c0` | `d(X1,Y1,Z0) − d(X1,Y0,Z0)` | `d(X1,Y1,Z1) − d(X1,Y1,Z0)` |
| 2 | `rx ≥ rz && rz ≥ ry` | `d(X1,Y0,Z0) − c0` | `d(X1,Y1,Z1) − d(X1,Y0,Z1)` | `d(X1,Y0,Z1) − d(X1,Y0,Z0)` |
| 3 | `rz ≥ rx && rx ≥ ry` | `d(X1,Y0,Z1) − d(X0,Y0,Z1)` | `d(X1,Y1,Z1) − d(X1,Y0,Z1)` | `d(X0,Y0,Z1) − c0` |
| 4 | `ry ≥ rx && rx ≥ rz` | `d(X1,Y1,Z0) − d(X0,Y1,Z0)` | `d(X0,Y1,Z0) − c0` | `d(X1,Y1,Z1) − d(X1,Y1,Z0)` |
| 5 | `ry ≥ rz && rz ≥ rx` | `d(X1,Y1,Z1) − d(X0,Y1,Z1)` | `d(X0,Y1,Z0) − c0` | `d(X0,Y1,Z1) − d(X0,Y1,Z0)` |
| 6 | `rz ≥ ry && ry ≥ rx` | `d(X1,Y1,Z1) − d(X0,Y1,Z1)` | `d(X0,Y1,Z1) − d(X0,Y0,Z1)` | `d(X0,Y0,Z1) − c0` |

lcms2 closes the cascade with an unreachable `c1 = c2 = c3 = 0` fallback (only NaN fractions
could reach it, and `fclamp` removes NaN first); since the six orderings are exhaustive for
finite fractions, branch 6 is the `else` arm in the Rust transcription.

**Dimension selection** (`cmsintrp.c:1178-1310`). Tetrahedral serves exactly 3 inputs unless
the `CMS_LERP_FLAGS_TRILINEAR` hint is set — which lcms2 sets only for **Lab-indexed** CLUTs
(`ChangeInterpolationToTrilinear`, `src/cmsio1.c:516-533`, applied to B2A/devicelink pipelines
whose PCS is Lab). Trilinear/bilinear LERP order is X, then Y, then Z
(`TrilinearInterpFloat`, `cmsintrp.c:470-540`). Four inputs and above
(`Eval4InputsFloat`…`Eval15InputsFloat`, `cmsintrp.c:1038-1174`) slice the outermost axis:
floor + fraction on input 0, evaluate the two inner (N−1)-D sub-grids, blend linearly —
bottoming out in the 3-D tetrahedral base.

## Transcription: black-point compensation and detection (#329)

From `third_party/lcms2` (lcms2 2.19); implemented by `gamut-cmm`'s `bpc`/`intent` modules
and `IccTransform::between` (`crates/gamut-cmm/src/{bpc,intent,transform}.rs`).

**The compensation formula** (`src/cmscnvrt.c:166-201`, `ComputeBlackPointCompensation` —
the ISO 18619 / WP40 method): a per-channel linear scaling in XYZ mapping the source black
point onto the destination's while fixing the D50 white,

```text
a = (bpOut − D50) / (bpIn − D50)        (diagonal matrix entries)
b = −D50 · (bpOut − bpIn) / (bpIn − D50)   (offset)
```

with **lcms2's rounded D50 literals** `cmsD50X/Y/Z = 0.9642, 1.0, 0.8249`
(`include/lcms2.h:292-294`) as the anchor — not the exact s15Fixed16 PCS illuminant. lcms2
divides the offset by `MAX_ENCODEABLE_XYZ` (1.99997) because its pipelines carry *encoded*
XYZ; `gamut-cmm`'s decoded pipelines use the offset as derived. BPC applies to the
non-absolute intents only (`cmscnvrt.c:1126-1127` forces the flag off for absolute), is
**forced on** for v4 profiles under perceptual/saturation (`_cmsLinkProfiles`,
`cmscnvrt.c:1119-1135` — per-slot, with only output-direction slots consumed), and is
skipped entirely when the two detected blacks are equal or the layer is within the
`IsEmptyLayer` tolerance (`Σ|m − I| + Σ|off| < 0.002`, `cmscnvrt.c:329-348`).

**The fixed v4 perceptual black** (`include/lcms2.h:297-299`):

```text
cmsPERCEPTUAL_BLACK_X = 0.00336   cmsPERCEPTUAL_BLACK_Y = 0.0034731   cmsPERCEPTUAL_BLACK_Z = 0.00287
```

**Detection** (`src/cmssamp.c` — there is no `cmsbpc.c`): the profile's `bkpt` tag is
deliberately ignored (`CMS_USE_PROFILE_BLACK_POINT_TAG` is compiled out by default — the tag
is bogus in too many real profiles); the black point is **estimated** instead:
`cmsDetectBlackPoint` (`cmssamp.c:238-323`) via class/intent gates, the fixed v4 perceptual
black, the ink-profile perceptual round trip, or the darker-colorant probe; and
`cmsDetectDestinationBlackPoint` (`cmssamp.c:399-598`) via the Adobe round-trip ramp
estimator (256-step L\* ramp, top-down monotonization, mid-range straightness shortcut,
least-squares quadratic root in the normalized shadow region — fit regions `[0.1, 0.5)`
relative, `[0.03, 0.25)` perceptual/saturation). Because the estimators run *transforms*
(f32 with 16-bit interpolation in lcms2, f64 in `gamut-cmm`), detected values agree between
implementations only to a tolerance — differential assertions against the oracle are
therefore **tolerance-based**, with the fixed-black and gate paths exact
(`crates/gamut-cmm/tests/oracle_intents.rs` records the measured bounds).

**Absolute rendering** (`src/cmscnvrt.c:249-325`, `ComputeAbsoluteIntent`): at the default
adaptation state 1.0 the adjustment is exactly `diag(WhiteIn/WhiteOut)` in XYZ, with the
media whites read through `_cmsReadMediaWhitePoint`'s quirks (`src/cmsio1.c:64-90`: missing
tag → D50; **v2 display-class profiles → forced D50**, tag ignored). The `chad` tag is
consumed *only* by the non-default adaptation-state branches, which `gamut-cmm` does not
implement.
