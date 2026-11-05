//! Tone-mapping math primitives for the gamut image-encoding workspace.
//!
//! Tone mapping compresses a high-dynamic-range (HDR) signal into a range a target display or
//! encoder can represent, trading absolute luminance for preserved relative contrast. This crate
//! provides the *math primitives* to do that — a small set of built-in tone curves plus an
//! extensible [`ToneCurve`] trait so consumers can supply their own — and deliberately stops
//! there: it performs no colour-space conversion, gamut mapping, or pixel I/O.
//!
//! # Items
//!
//! Every item lives at the crate root, its one public path.
//!
//! - [`ToneCurve`] — the trait every operator implements, with an in-place slice helper and a
//!   blanket impl so a plain `Fn(f32) -> f32` closure is itself a curve.
//! - The built-in curves: [`Reinhard`] / [`ReinhardExtended`], the filmic [`Aces`] and [`Hable`],
//!   [`Drago`] logarithmic mapping, [`Exposure`] pre-scaling, the [`Linear`] passthrough, and
//!   [`Clamp`].
//! - Per-operator parameter defaults: [`SDR_WHITE_NORMALIZED`], [`DEFAULT_REINHARD_WHITE`],
//!   [`DEFAULT_HABLE_WHITE`] and [`DEFAULT_DRAGO_BIAS`]. (Absolute reference luminances in nits
//!   live in [`gamut_core::luminance`].)
//!
//! # Inputs and the HDR-to-SDR pipeline
//!
//! Curves take a single non-negative *linear-light* value — a per-channel RGB component or a
//! luminance. A full HDR-to-SDR conversion is three steps, of which this crate owns the middle one:
//!
//! 1. Linearize the source signal, identified by its transfer function (e.g. `Pq` or `Hlg` from
//!    `gamut_color`'s `TransferCharacteristics`).
//! 2. Apply a [`ToneCurve`] from this crate to the linear values.
//! 3. Re-encode through the target SDR transfer function (e.g. `Srgb`).
//!
//! Keeping the boundary here means a curve is just `f32 -> f32` and reusable outside any colour
//! pipeline.
//!
//! # Example
//!
//! ```
//! use gamut_tonemap::{ReinhardExtended, ToneCurve};
//!
//! let curve = ReinhardExtended::new(4.0)?; // linear white point = 4.0
//! let display = curve.map(2.5);
//! assert!(display > 0.0 && display < 1.0);
//!
//! // Any closure is also a curve:
//! let gamma = |x: f32| x.powf(1.0 / 2.2);
//! let mut linear = [0.2_f32, 0.8, 3.0];
//! gamma.map_slice(&mut linear);
//! # Ok::<(), gamut_core::Error>(())
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod constants;
mod curve;
mod operators;

// Executable laws for the `ToneCurve` contract, shared by this crate's property tests and the
// out-of-tree fuzz tier (`test-support`). Not part of the stable API; see `docs/testing.md`.
// A plain comment, not `///`: outer docs here would make rustdoc resolve the module's own `//!`
// intra-doc links from the crate root, where its items are not in scope.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;

pub use constants::{
    DEFAULT_DRAGO_BIAS, DEFAULT_HABLE_WHITE, DEFAULT_REINHARD_WHITE, SDR_WHITE_NORMALIZED,
};
pub use curve::ToneCurve;
pub use operators::{Aces, Clamp, Drago, Exposure, Hable, Linear, Reinhard, ReinhardExtended};
