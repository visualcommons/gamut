# gamut ICC sandbox

An interactive egui/eframe app (issue #247) that runs ICC colour management end to end through
[`gamut-icc`](../../crates/gamut-icc) (parse, inspect, serialize) and
[`gamut-cmm`](../../crates/gamut-cmm) (link, transform, proof, gamut-check), natively or in a
browser canvas. It is the flagship example for both crates, and each panel is one small use of
one public API.

| Task | What it does |
| --- | --- |
| `mise run icc-sandbox` | Opens the native window. |
| `mise run icc-sandbox-web` | Builds the wasm32 app and its page into `target/web`; serve that directory over HTTP. |
| `mise run test-icc-sandbox` | Runs the model's tests and the lcms2 conformance test. No window opens. |
| `mise run check-icc-sandbox` | Runs Clippy over the native and wasm32 builds. |

The native tasks link Little-CMS through `tooling/lcms2-oracle`, so they need the
`third_party/lcms2` submodule (`git submodule update --init third_party/lcms2`) and a C
compiler. The crate is excluded from the workspace, so `cargo test --workspace` and the
coverage gate never build egui.

## Tabs

| Tab | Scenario or panel | Library entry points |
| --- | --- | --- |
| Formats | One chart tagged sRGB or Display P3, embedded in PNG (`iCCP`) and JPEG (APP2), decoded, extracted and rendered | `gamut_png`/`gamut_jpeg` encoders and `metadata`, `IccProfile::parse`, `IccTransform::between` |
| Camera | One 16-bit raw capture read through two camera (`scnr`) profiles, with per-patch ΔE₀₀ to the scene | `transform_interleaved_u16`, `device_to_pcs` |
| Print | A v4 CMYK printer: ink plates, a soft proof, and a gamut warning at all four intents, with BPC on or off | `IccTransform::proofing`, `GamutCheck` |
| Tag inspector | The header, the tag table, the colorant/`wtpt`/`chad` matrices, and `validate()` findings | `IccProfile::get`, `IccProfile::validate` |
| TRC curves | Every tone-curve tag plotted forward and inverse, with the round-trip error | `ToneCurve::{new, eval, inverse, is_monotonic}` |
| CLUT viewer | A 2-D slice through any CLUT, tetrahedral or multilinear | `ClutTable::with_interpolation`, `Stage::Clut` |
| Gamut hull | Device-surface samples in CIE xy, plus PCS → device → PCS round-trip ΔE₀₀ | `device_to_pcs`, `pcs_to_device` |
| Malformed profile | A truncated profile: the parser's error, the declared size against the bytes present, and the untagged fallback | `IccProfile::parse` |
| Conformance | Every rendered path cross-checked against lcms2, summarized by the max-ΔE₀₀ badge in the top bar | (native builds only) |

## Profiles

Every profile is **synthesized at runtime** with `IccWriter` and parsed back. None is a
committed file. The battery holds a v2 sRGB with a sampled TRC, the built-in v4 sRGB (the
display profile) and Display P3, two camera profiles (matrix/TRC over XYZ, and `lutAToB` over
Lab), a v4 CMYK printer with `A2Bx`/`B2Ax`/`gamt` tables and a non-D50 paper white, and the
truncated sRGB. The camera and printer models are synthetic. They are built to drive each code
path and are not measurements of real hardware. `src/profiles.rs` documents each one.

## Display path

Every pixel on screen is converted to sRGB by `gamut-cmm` and handed to egui untagged, so
neither the OS nor the browser colour-manages it a second time.

## Conformance

The badge compares `gamut-cmm` (`f64`, stage by stage) with lcms2's full-precision path
(`TYPE_*_DBL`, `NOOPTIMIZE | NOCACHE`), measured in ΔE₀₀ through the destination's Lab lens.
Its bound is `gamut-cmm`'s own TIGHT LUT bound, 2e-2. The wasm build has no badge, because
lcms2 is C.

## Known limits

- `gamut_icc::IccError` carries one static message, with no byte offset and no tag identity.
  The malformed-profile panel adds the one fact it can check on its own, the header's declared
  size against the bytes present. Structured context in `IccError` is tracked in #1004.
- The badge leaves out one pair that no panel renders, the printer into the v2 sampled-TRC sRGB.
  It measures 3.51e-2 there, over the bound. #1003 traced that excess to lcms2's 16-bit
  evaluation of the reversed sampled TRC, not to `gamut-cmm`, which gates the pair under its own
  class bound, 8e-2.
- The browser page needs a one-line JavaScript module loader. `wasm-bindgen` generates that
  glue, and no npm, bundler, or hand-written JavaScript is involved.
