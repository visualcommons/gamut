//! ICC colour management module (CMM) for the gamut image-encoding workspace.
//!
//! A CMM turns parsed ICC profiles into runnable colour transforms: it links a source and a
//! destination profile into one device→PCS→device conversion and evaluates it over pixels.
//! This crate provides the transform *engine* — the [`Pipeline`]/[`Stage`] evaluation model
//! and the object-safe [`Transform`] entry trait — over profiles parsed by
//! [`gamut-icc`](https://crates.io/crates/gamut-icc), with colorimetric primitives from
//! [`gamut-color`](https://crates.io/crates/gamut-color). It covers the **ICC v2/v4
//! still-image profile set** and deliberately stops there: iccMAX (`ICC.2`),
//! `multiProcessElementsType` (`mpet`), and the `DToBx`/`BToDx` transform tags are out of
//! scope (see `references/icc/README.md` — the profiles embedded in real images are all
//! ICC.1 v2/v4, and the oracle below does not implement iccMAX).
//!
//! # Numeric domain
//!
//! Evaluation is scalar **`f64` throughout**, at **Tier-1 (correctness only)**: results are
//! correct to specification but not bit-reproducible across platforms — the same posture as
//! `gamut-color` (see `references/color/README.md`). Samples are interleaved per pixel.
//! **Device channels are encoded values in `[0.0, 1.0]`** (an 8-bit device sample `s` enters
//! as `s / 255`); **PCS seams are decoded colorimetry** — PCSXYZ carries XYZ with D50
//! luminance `Y = 1.0`, PCSLAB carries `L*` in `0..=100` and `a*`/`b*` in their natural
//! signed range. This convention governs every stage this crate ever evaluates.
//!
//! Behavioural oracle: **Little-CMS (lcms2)**, linked dev-only via `tooling/lcms2-oracle`
//! (see `references/cmm/README.md`) — ICC.1 specifies data layouts, not CMM behaviour, so
//! where the spec is silent (interpolation, clamping) observable semantics follow lcms2.
//!
//! # Modules
//!
//! - [`pipeline`] — the keystone: [`Stage`] (the evaluation primitive) and [`Pipeline`] (a
//!   validated chain of stages), plus the [`MAX_CHANNELS`] bound that keeps evaluation
//!   allocation-free.
//! - [`curve`] — [`ToneCurve`]: 1-D tone-curve evaluation, monotonicity detection, and
//!   inversion over `gamut-icc`'s parsed `curveType`/`parametricCurveType` elements, applied
//!   per channel by [`Stage::Curves`].
//! - [`clut`] — [`ClutTable`]: multi-dimensional CLUT interpolation (lcms2's tetrahedral
//!   decomposition and N-D multilinear, selectable via [`ClutInterpolation`]) over
//!   `gamut-icc`'s parsed CLUT elements, applied by [`Stage::Clut`].
//! - [`link`] — profile linking: [`device_to_pcs`]/[`pcs_to_device`] build runnable pipelines
//!   from parsed profiles — LUT profiles (`lut8`/`lut16`/`lutAToB`/`lutBToA` tags, selected
//!   per rendering intent with lcms2's intent tables and perceptual fallback) and matrix/TRC
//!   shaper profiles; the module docs record the dispatch rule and the settled
//!   `chad`/colorant convention.
//! - [`transform`] — the object-safe [`Transform`] entry trait every runnable transform
//!   implements, and [`IccTransform`]/[`TransformOptions`]: two profiles linked end to end
//!   at a rendering intent, with the ICC-absolute white scaling and black-point compensation
//!   applied at the PCS seam.
//! - [`bpc`] — black-point **detection** ([`detect_black_point`],
//!   [`detect_destination_black_point`] — lcms2's estimators transcribed) and the
//!   compensation scaling [`IccTransform`] applies.
//! - [`error`] — the typed [`CmmError`] and the crate [`Result`].
//!
//! # Pipeline placement
//!
//! `gamut-cmm` sits between `gamut-icc` (which parses/serializes the profile blob a format
//! crate extracts) and pixel data: a format crate or application hands the parsed profiles to
//! this crate, receives a [`Transform`], and runs it over decoded samples. Profile parsing
//! stays in `gamut-icc`; CICP signaling and transfer functions stay in `gamut-color`.
//!
//! # Example
//!
//! ```
//! use gamut_cmm::{Pipeline, Stage, Transform};
//!
//! // A toy transform: scale-and-offset each RGB channel, then clamp to [0, 1].
//! let scale = Stage::Matrix {
//!     m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
//!     offset: [0.25, 0.25, 0.25],
//! };
//! let pipeline = Pipeline::new(3, 3, vec![scale, Stage::Clamp { channels: 3 }])?;
//!
//! let src = [1.0, 0.5, 2.0]; // one interleaved RGB pixel, encoded [0, 1]
//! let mut dst = [0.0; 3];
//! pipeline.transform(&src, &mut dst)?;
//! assert_eq!(dst, [0.75, 0.5, 1.0]); // 2.0 → 1.25 → clamped to 1.0
//! # Ok::<(), gamut_cmm::CmmError>(())
//! ```
#![forbid(unsafe_code)]

pub mod bpc;
pub mod clut;
pub mod curve;
pub mod error;
mod intent;
pub mod link;
pub mod pipeline;
pub mod transform;

#[doc(inline)]
pub use bpc::{detect_black_point, detect_destination_black_point};
#[doc(inline)]
pub use clut::{ClutInterpolation, ClutTable};
#[doc(inline)]
pub use curve::ToneCurve;
#[doc(inline)]
pub use error::{CmmError, Result};
#[doc(inline)]
pub use link::{device_to_pcs, pcs_to_device};
#[doc(inline)]
pub use pipeline::{MAX_CHANNELS, Pipeline, Stage};
#[doc(inline)]
pub use transform::{IccTransform, Transform, TransformOptions};
