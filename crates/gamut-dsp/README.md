# gamut-dsp

`gamut-dsp` holds the shared digital signal processing kernels for the gamut codecs —
spec-exact transform kernels and quantization rounding, implemented once and consumed by the
format crates.

The surface is one module per spec family, plus the shared integer vocabulary:

- **`av1`** — the AV1 §7.13.2 transform kernels: the 1-D inverse/forward DCT (lengths 4–64),
  ADST (4/8/16 — DST-VII at size 4 and DST-IV at 8/16, a genuine quirk of the spec), identity
  transforms (4–32), and the complete lossless 4×4 Walsh–Hadamard block pair. The `inverse_*`
  kernels are the normative, bit-exact decoder processes; the `forward_*` kernels are encoder
  choices guaranteed consistent with them. The 2-D row/column assembly (AV1 §7.13.3) lives with
  the codec, in `gamut-av1`.
- **`jpeg`** — the ITU-T T.81 §A.3.3 8×8 DCT: the reference forward/inverse pair
  (`forward_dct8x8` / `inverse_dct8x8`, separable `f64` evaluations rounded once) and the fast
  13-bit fixed-point inverse `gamut-jpeg`'s decoder runs (`inverse_dct8x8_int`, bit-exact with
  libjpeg-turbo's `jpeg_idct_islow`, plus its DC-only shortcut `inverse_dct8x8_int_dc`). Level
  shift, quantization and zig-zag ordering live with the codec, in `gamut-jpeg`.
- **`math`** — the cross-codec integer primitives: the AV1 §4.7 rounding/clamp operations
  (`round2`, `round2_signed`, `clip3` — names every codec spec defines equivalents of) and the
  forward-quantize rounding shared by the AV1 and VP8 encoders (`round_div_nearest`).

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Keep the shared math in one place.** A kernel consumed by several codecs is implemented and
  tested once here, so a numerical bug is fixed for every format at once. Kernels used by a
  single format deliberately stay in that format's crate (VP8 in `gamut-webp`, PNG filters in
  `gamut-png`, the TIFF predictor in `gamut-tiff`) until a second consumer exists.
- **Track the spec exactly.** Routines are implemented clean-slate from the official specs (see
  [`../../references/`](../../references)); the normative inverse kernels are bit-exact, pinned
  by exact golden vectors in-crate and by the dav1d/libaom conformance cross-checks through the
  2-D pipeline.
- **Stay pure.** Every function is total, deterministic math on caller memory — in-place slices
  or fixed-size arrays, no allocation, **zero dependencies**, no `unsafe`.

## Usage

```rust
use gamut_dsp::av1::{forward_wht4x4, inverse_wht4x4};
use gamut_dsp::math::round_div_nearest;

// The lossless 4×4 Walsh–Hadamard pair round-trips exactly.
let residual = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
let coeffs = forward_wht4x4(&residual);
assert_eq!(inverse_wht4x4(&coeffs), residual);

// The shared forward-quantize rounding divides to the nearest level, ties away from zero.
assert_eq!(round_div_nearest(-10, 4), -3);
```

## Status

**Stable at the current major.** The complete AV1 1-D kernel family (DCT 4–64, ADST
4/8/16, identity 4–32, lossless WHT), the JPEG 8×8 DCT kernels, and the shared integer math ship
today and their signatures will not change; every future extension is additive — a new sibling
module per spec family, or new functions in an existing one. See [`STATUS.md`](STATUS.md) for the
frozen surface table, the settled design decisions, and the tracked deferrals.

## Roadmap (all additive)

- **`jxl`** / **`av2`** — kernels for the JPEG XL and AV2 codecs as those crates grow real
  surfaces; neither declares a `gamut-dsp` dependency today.
- **SIMD** variants behind the same signatures, where the benches justify them — an internal,
  non-breaking optimization.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
