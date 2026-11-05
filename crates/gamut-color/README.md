# gamut-color

`gamut-color` holds the color primitives the gamut codecs share: pixel formats, bit depths, chroma
subsampling, the CICP code points carried in nclx / AV1 sequence headers, planar pixel buffers, and
the `f64` colour-science layer (transfer functions, OKLab, gamut mapping, source profiles).

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Centralize color management.** One place defines [`BitDepth`], [`ChromaSubsampling`], the CICP
  enums ([`ColourPrimaries`], [`TransferCharacteristics`], [`MatrixCoefficients`], [`ColorRange`]),
  and the [`Planar8`] / [`Planar16`] buffers — so a color bug is fixed once, not re-implemented
  per format. (The interleaved pixel layouts are the typed `Pixel` markers in `gamut-core`.)
- **Model the full spec surface up front.** The enums cover every AV1 bit depth and subsampling and
  the CICP code points, so a code point no encode path emits yet is still representable and
  extension does not reshape the types.
- **Match the spec code points exactly.** CICP values mirror the H.273 / AV1 sequence-header code
  points so they round-trip through `av1C`/`colr` and AV1 headers unchanged.
- **Provide the colour science.** The `transfer` (encoder-exact EOTFs), `oklab` (per-gamut OKLab
  transforms), `lab` (CIELab/LCh/xyY with exact-rational ε/κ, the ICC PCS fixed-point encodings,
  and the ΔE\*ab / CIEDE2000 colour-difference metrics), `matrix` (RGB↔XYZ via Bradford
  adaptation), `gamut_map` (hue-preserving soft clamp), `profile` (source bundles), and `linalg`
  (the shared 3×3 helpers) modules. This math is **Tier-1** (correctness only): it uses `std`
  `f64`, so it is not bit-reproducible across platforms — see
  [`references/color/README.md`](../../references/color/README.md).
- **Stay memory-safe.** 100% safe Rust (`#![deny(unsafe_code)]`).

## Usage

```rust
use gamut_color::Planar8;

// 8-bit interleaved RGB -> identity (mc = 0) 4:4:4 planes (Y=G, U=B, V=R).
let width = 2;
let height = 2;
let rgb: Vec<u8> = vec![0; width * height * 3];
let planes = Planar8::from_rgb8_identity(&rgb, width as u32, height as u32).expect("valid input");
assert_eq!(planes.width(), 2);
let _y = planes.plane(0); // luma plane
```

Enable the optional `serde` feature to serialize and deserialize [`BitDepth`],
[`ChromaSubsampling`], and the four CICP enums by their Rust variant names. It also enables
`gamut-core`'s matching feature and is disabled by default; the numeric CICP code points remain the
explicit `code_point`/`from_code_point` API rather than serde's representation.

## Status

Stable at the current major (first released as 1.0 under issue #179); see [`STATUS.md`](STATUS.md) for the phase history, the frozen
API policies, and the deferrals. Implemented today: 8-bit RGB ↔ identity 4:4:4 conversion
([`Planar8::from_rgb8_identity`] / [`Planar8::to_rgb8_identity`]), the CICP tables the AVIF
`colr` box needs, the BT.601 YCbCr 4:2:0 path (WebP), the decoders' reference-exact chroma
upsampling (`upsample`: libjpeg-turbo, libwebp and libavif kernels behind `ChromaUpsampling`), the
general H.273 §8.3 luma–chroma transform in both directions (`RgbToYcbcr` / `YcbcrMatrix`: BT.601 / BT.470 B,G / BT.709 / BT.2020-NCL, both
signal ranges, at every modeled bit depth — the AVIF lossy encode path and the AVIF/HEIC
presentation path), the `f64` colour science for the sRGB, Display P3, Adobe RGB, BT.2020 and
ProPhoto gamuts, and the CIELab / ΔE layer (issue #321: XYZ↔Lab↔LCh, xyY, the ICC PCS encodings,
CIE76 and CIEDE2000 — the latter pinned to the Sharma 34-pair golden set, with lcms2 differential
tests to follow in issue #322), the 4:2:2 / 4:2:0 [`Planar8`] geometries, the 10/12-bit
[`Planar16`] planes, and monochrome (`Cs400`) grey/alpha planes in both buffers — all coded end to
end by `gamut-avif`. `YCgCo` is modeled
without a conversion; the enums are `#[non_exhaustive]`, so extension is non-breaking. See the
crate docs ("Implemented vs. modeled") for the precise split.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
