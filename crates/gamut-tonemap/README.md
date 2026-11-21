# gamut-tonemap

`gamut-tonemap` provides the mathematical primitives to tone map HDR signals into a target range,
with a small set of built-in tone curves and an extension trait so you can plug in your own.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide tone-mapping math that is:

- **Memory-safe.** `#![forbid(unsafe_code)]`; pure `f32` math, no I/O.
- **Clean-slate and self-contained.** Built-in operators are implemented directly from their
  published definitions (Reinhard et al.; ACES/Narkowicz; Hable; Drago et al.), transcribed in
  [`references/tonemap`](../../references/tonemap/README.md), not wrapped from a C library.
- **Layered on shared crates.** Built on [`gamut-core`](../gamut-core) and designed to slot between
  [`gamut-color`](../gamut-color)'s transfer-function handling and an SDR encoder.

## Use cases

- **HDR-to-SDR conversion** before encoding to an SDR-only target.
- **Display adaptation** — mapping mastered content to a display's peak luminance.
- **Custom operators** — define a curve once, as a closure or a type, and reuse it across a pipeline.
- **Hue-preserving mapping** — drive a curve from a pixel's luminance or brightest channel, with
  ffmpeg `tonemap`-style desaturation, between the source's and the display's peaks.

## Integration with other gamut libraries

Tone curves operate on **non-negative linear-light** values. The end-to-end HDR→SDR path is:

1. Decode the encoded signal and linearize it. The source transfer function is identified by
   `gamut_color`'s `TransferCharacteristics` (e.g. `Pq`, `Hlg`).
2. Apply a `ToneCurve` from this crate to the linear values.
3. Re-encode through the target SDR transfer function (e.g. `Srgb`).

This crate owns step 2 only; linearization and re-encoding live in [`gamut-color`](../gamut-color).
Keeping the boundary here means a curve is just `f32 -> f32` and reusable outside any colour pipeline.
Where a scalar curve per channel would shift hue, step 2 is a `ToneMapper` over the whole pixel
instead.

## Usage

```rust
use gamut_tonemap::{Aces, ReinhardExtended, ToneCurve};

// Built-in operator with a white point (the linear value that maps to display white).
let curve = ReinhardExtended::new(4.0)?;
let display = curve.map(2.5);

// Filmic operators are parameterless:
let filmic = Aces.map(2.5);

// Any closure is also a curve, applied in place over a slice.
let gamma = |x: f32| x.powf(1.0 / 2.2);
let mut linear = [0.2_f32, 0.8, 3.0];
gamma.map_slice(&mut linear);
# Ok::<(), gamut_core::Error>(())
```

RGB-aware mapping applies any curve to whole pixels. The defaults are ffmpeg `tonemap`'s:
max-RGB application with `desat = 2`, mapping the source peak onto `1.0`.

```rust
use gamut_core::luminance::BT2020_LUMA_WEIGHTS;
use gamut_tonemap::{ApplyMode, CurveToneMapper, Hable, ToneMapper};

// A 1000-nit BT.2020 master (linear, 1.0 = 100 cd/m²) into SDR, driven by luminance.
let mapper = CurveToneMapper::new(Hable::default(), 10.0)?
    .with_luma_weights(BT2020_LUMA_WEIGHTS)?
    .with_mode(ApplyMode::Luminance);
let sdr = mapper.map_rgb([4.0, 2.0, 0.5]);
assert!(sdr.iter().all(|c| (0.0..=1.0).contains(c)));
# Ok::<(), gamut_core::Error>(())
```

A runtime-selected operator, e.g. from a config file or across a C boundary, is a plain-data
`ToneOperator` plus a `Copy` `ToneParams` record, built into the same mapper:

```rust
use gamut_tonemap::{ToneMapper, ToneOperator, ToneParams};

let mapper = ToneParams::new(ToneOperator::Drago, 100.0, 10.0) // Drago, world_max = 100
    .with_bias(0.7)
    .mapper()?;
let sdr = mapper.map_rgb([4.0, 2.0, 0.5]);
assert!(sdr.iter().all(|c| c.is_finite()));
# Ok::<(), gamut_core::Error>(())
```

## Status

The `ToneCurve` trait (with a blanket impl for any `Fn(f32) -> f32`) and eight built-in operators —
`Linear`, `Clamp`, `Exposure`, `Reinhard`, `ReinhardExtended`, `Aces`, `Hable`, and `Drago` — plus
their parameter-default constants; the RGB-aware `ToneMapper` trait with `CurveToneMapper`,
`ApplyMode` and `DEFAULT_DESAT` (#1236); and the plain-data `ToneOperator` selector with
`ToneParams` and `BuiltinCurve` (#1103). Every item is at the crate root, its one public path.
Each is implemented clean-slate from the primary source documented in
[`references/tonemap`](../../references/tonemap/README.md); `CurveToneMapper`'s desaturation and
max-RGB application transcribe ffmpeg's `tonemap` filter and are held to it by a golden test.
Reachable through the umbrella crate's `tonemap` feature. The v1 surface is **frozen** (issue
#188), and the `ToneMapper` additions extend it without changing it; see [STATUS.md](STATUS.md)
for the surface table and settled design decisions.

## Deferred

Planned but not yet implemented: a turnkey HDR→SDR helper pairing curves with `gamut-color` transfer
functions, curve-composition combinators, and the BT.2390 EETF (#1254) and BT.2446 Method A (#1255)
as `ToneMapper`s. See [STATUS.md](STATUS.md).

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
