# gamut-tonemap

`gamut-tonemap` provides the mathematical primitives to tone map HDR signals into a target range,
with a small set of built-in tone curves and an extension trait so you can plug in your own.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide tone-mapping math that is:

- **Memory-safe.** `#![forbid(unsafe_code)]`; pure scalar `f32` math, no I/O.
- **Clean-slate and self-contained.** Built-in operators are implemented directly from their
  published definitions (Reinhard et al.; ACES/Narkowicz; Hable; Drago et al.), transcribed in
  [`references/tonemap`](../../references/tonemap/README.md), not wrapped from a C library.
- **Layered on shared crates.** Built on [`gamut-core`](../gamut-core) and designed to slot between
  [`gamut-color`](../gamut-color)'s transfer-function handling and an SDR encoder.

## Use cases

- **HDR-to-SDR conversion** before encoding to an SDR-only target.
- **Display adaptation** — mapping mastered content to a display's peak luminance.
- **Custom operators** — define a curve once, as a closure or a type, and reuse it across a pipeline.

## Integration with other gamut libraries

Tone curves operate on **non-negative linear-light** values. The end-to-end HDR→SDR path is:

1. Decode the encoded signal and linearize it. The source transfer function is identified by
   `gamut_color`'s `TransferCharacteristics` (e.g. `Pq`, `Hlg`).
2. Apply a `ToneCurve` from this crate to the linear values.
3. Re-encode through the target SDR transfer function (e.g. `Srgb`).

This crate owns step 2 only; linearization and re-encoding live in [`gamut-color`](../gamut-color).
Keeping the boundary here means a curve is just `f32 -> f32` and reusable outside any colour pipeline.

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

## Status

The `ToneCurve` trait (with a blanket impl for any `Fn(f32) -> f32`) and eight built-in operators —
`Linear`, `Clamp`, `Exposure`, `Reinhard`, `ReinhardExtended`, `Aces`, `Hable`, and `Drago` — all
re-exported at the crate root. Each is implemented clean-slate from the primary source documented in
[`references/tonemap`](../../references/tonemap/README.md). Reachable through the umbrella crate's
`tonemap` feature. The public surface is **frozen at v1** (issue #188); see [STATUS.md](STATUS.md)
for the surface table and settled design decisions.

## Deferred

Planned but not yet implemented: a turnkey HDR→SDR helper pairing curves with `gamut-color` transfer
functions, and curve-composition combinators. See [STATUS.md](STATUS.md).

## License

Licensed under either of MIT or Apache-2.0 at your option.
