# gamut-color — color primitives implementation status

The shared color layer of the [gamut](../../README.md) codec campaign: CICP (H.273 /
ISO/IEC 23091-2) code points, coded-plane metadata (AV1 §5.5.2/§6.4.2), identity and BT.601
YCbCr 4:2:0 planar buffers (RFC 6386 §9.2 / libwebp), and the Tier-1 `f64` colour science
documented in [`references/color`](../../references/color/README.md). Released as **v1**
(issue #179).

**Keystone (done):** the **derivation-vs-literal `M1` verification** plus the **encoder-exact
transfer set** — every per-gamut OKLab `M1` table is re-derived from CIE chromaticities via
Lindbloom's RGB→XYZ construction and Bradford adaptation and must match the hand-transcribed
literals to `1e-7` (`derived_matrices_match_literals`), so no colour constant is trusted as a
copy-paste; and the transfer layer exposes both the encoder-exact curves (Adobe `x^2.2`,
ProPhoto `x^1.8`, BT.2020 PQ→Reinhard@203) and the standard curves, pinned to differ.

**Oracle:** published external values, no FFI — Bruce Lindbloom's sRGB→XYZ (D65) and Bradford
D65→D50 matrices; Ottosson's published OKLab `M1`/`M2` constants; an independent BT.2100
forward-OETF inversion of `pq_eotf`; chromahash golden vectors (`unit-color.json`,
`unit-softgamutclamp.json`, MIT OR Apache-2.0); libwebp's integer-exact `VP8RGBToY/U/V`
anchors plus the JFIF/BT.601 full-range anchors; and for the `lab` module the Sharma, Wu &
Dalal (2005) 34-pair CIEDE2000 golden set (`references/color/ciede2000-testdata.txt`, asserted
to 1e-4 in both argument orders), with lcms2 differential tests as the follow-up oracle
(issue #322 — the PCS codecs already replicate lcms2 `cmspcs.c` rounding/clamping exactly).
Determinism is **Tier-1** (correctness only, `std` `f64`): golden-vector agreement is
tolerance-level, not bit-for-bit.

## Phases

| Phase | Scope | Status |
| ----- | ----- | ------ |
| P1 | Scaffold + CICP/format tables: H.273 code-point enums, `BitDepth`, `ChromaSubsampling` | ✅ |
| P2 | `Planar8` identity (`mc = 0`, GBR) 4:4:4 buffers for the AV1/AVIF path | ✅ |
| P3 | BT.601 YCbCr 4:2:0 (`Yuv420`) with libwebp-exact limited-range integer math (WebP/VP8) | ✅ |
| P4 | Colour science: `transfer`, `oklab`, `matrix` (Bradford), `gamut_map`, `profile` | ✅ |
| P5 | **Keystone** — `M1` derivation cross-check + encoder-exact vs standard curves | ✅ |
| P6 | Tone-map dedupe with `gamut-tonemap`; luminance levels sourced from `gamut_core::luminance` | ✅ |
| v1 | Issue #179 — API finalization (`non_exhaustive` policy, profile surface, code-point inverses), overflow-safe constructors, AV1 §6.4.2 monochrome fix, oracle-only minimal test set | ✅ |
| P7 | `xyb` (issue #334): the JPEG XL opsin space — frozen libjxl 0.12.0 constants (absorbance matrix + bias + normative inverse, transcribed in `references/color/README.md` with `references/jxl/opsin_params.h` vendored), `linear_srgb_to_xyb`/`xyb_to_linear_srgb`, and the scaled-XYB byte encoding (`scale_xyb`/`unscale_xyb`, third stored channel `B − Y`) for `gamut-jpeg`'s XYB colour mode. Tier-1 f64 like the rest of the colour science | ✅ |
| P8 | Issue #321 — CIELab colorimetry (`lab`): XYZ↔Lab (exact ε = 216/24389, κ = 24389/27), LCh, xyY, ICC PCS encodings (PCSXYZ u1Fixed15, 16-bit PCSLAB v4/v2, 8-bit Lab; lcms2 `cmspcs.c`-exact rounding/clamping), ΔE\*ab + CIEDE2000; `linalg` promoted public for gamut-cmm (#323/#327) | ✅ |
| P9 | Issue #349 — `cct`: correlated colour temperature of a CIE 1931 chromaticity by Robertson's method (Wyszecki & Stiles isotemperature table, interpolated in mired). The definition raw formats build on: DNG 1.7.1 weights a camera's colour calibrations by inverse CCT, so `gamut-dng` needs it to read `AsShotWhiteXY`. Tier-1 f64 like the rest of the colour science | ✅ |

## API policies frozen at v1

- **Per-source-spec naming.** H.273 identifiers keep the spec's British spelling
  (`ColourPrimaries`); AV1-derived names use American spelling (`ColorRange`, `clip_pixel`).
- **Growth-bound enums are `#[non_exhaustive]`** (CICP code points, `BitDepth`,
  `ChromaSubsampling`, `Gamut`, `SourceTransfer`); `ColorRange` is deliberately exhaustive (a
  spec-complete single bit). New variants and `from_code_point` `None`→`Some` promotions are
  minor releases.
- **`gamut_core::Error` is the error surface** — the crate's only fallible paths are
  buffer-shape validation, exactly `InvalidInput`-shaped; no crate-local error enum.
- **Range is a conversion property, not buffer state**: `Yuv420` does not store the
  `ColorRange` used to fill it; callers carry the flag end to end (it is header metadata).
- **`oklab_to_linear_srgb` is sRGB-only by design** — sRGB is the gamut-membership test target
  and the encode target; per-gamut inverses are additive later if a use case appears.

## Intentionally deferred (additive)

- **10/12-bit plane wiring** — both H.273 directions now cover 8/10/12/16-bit (`RgbToYcbcr` /
  `YcbcrMatrix`); what is deferred is the plane geometry to carry them. `BitDepth::Ten`/`Twelve`
  are modeled and the AV1 reconstruction accepts them, but `Planar8` is 8-bit and no encode path
  produces a deeper plane yet (gamut-avif M2).
- **16-bit is modeled, not an AV1 depth** — `BitDepth::Sixteen` (issue #260) is not deferred AV1
  wiring like `Ten`/`Twelve`: AV1 tops out at 12 and never produces it. It exists for the 16-bit
  interleaved still-image pipelines (PNG/TIFF/JXL/DNG) that consume this vocabulary. `clip_pixel`
  accepts it; no gamut encode path emits it. Depths outside the fixed 8/10/12/16 set (RAW's
  14-bit, TIFF's `1..=16`) are deliberately **not** enum variants — those formats carry a
  free-form integer depth (`gamut-dng`'s `bits_per_sample`, validated `1..=16`), a domain an enum
  cannot model.
- **Subsampled coded planes** — `Cs422`/`Cs420`/`Cs400` wiring into AV1 (M2); today 4:2:0
  exists only in the WebP/VP8 `Yuv420` path.
- **`MatrixCoefficients::YCgCo`** — modeled, with neither direction yet: it is a lifting transform,
  not a `Kr`/`Kb` matrix. Lands with M4. `Identity` is deliberately *not* a `YcbcrMatrix` either —
  it is a GBR plane permutation the caller performs, not an affine transform — and `Unspecified` is
  a policy choice belonging to the format layer.
- **Row / SIMD YCbCr ↔ RGB** — `YcbcrMatrix::to_rgb` and `RgbToYcbcr::from_rgb` are scalar and
  `#[inline]`; a subsampling-aware row API is additive if profiling asks for it.
- **HLG display OOTF** — the HLG transfer (H.273 code point 18) is implemented scene-referred, as
  `hlg_oetf` / `hlg_inverse_oetf` (issue #555); the BT.2100 HLG *EOTF*, which adds a system gamma
  set by the display's peak luminance, is not modelled. The BT.709-family curve (H.273 code
  points 1 and 14) is implemented as `bt709_eotf` / `bt709_oetf`; code points 6 and 15, the same
  curve per Table 3, have no `TransferCharacteristics` variant yet (#675). The display reading
  H.273 §8.2 NOTE 1 names for them, the Rec. ITU-R BT.1886 EOTF, is `bt1886_eotf` at reference
  black zero only (`V^2.4`, issue #586); `eotf_for` keeps the inverse OETF, and BT.1886's
  non-zero-black form (`Lb > 0`) is not modelled.
- **Better chroma resampling** (sharp-YUV-style) — the box filter is the documented baseline.
- **Bit-reproducible math substrate** — chromahash's `cbrt_halley`/portable-pow tier is
  deliberately not ported (Tier-1 policy, issue #37).
- **ΔE94 and ΔE-CMC** — deliberately out of scope (issue #321): CIEDE2000 supersedes both for
  the workspace's conformance-metric and gamut-statistics use cases; either can be added to
  `lab` later without reshaping the module.
- **lcms2 differential tests for the `lab` module** — issue #322; the PCS codecs are already
  written to lcms2 `cmspcs.c`'s exact rounding and clamping so the differential can demand
  exactness rather than tolerance.
