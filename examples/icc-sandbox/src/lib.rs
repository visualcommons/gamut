//! The interactive ICC sandbox (issue #247): the flagship example for
//! [`gamut-icc`](gamut_icc) and [`gamut-cmm`](gamut_cmm).
//!
//! The crate is two layers. This library is the **model** — UI-free, so its tests run without a
//! windowing stack — and every one of its modules is one small, standalone use of a library
//! primitive that doubles as documentation for it:
//!
//! - [`profiles`] — the profile battery, **synthesized at runtime** with [`gamut_icc::IccWriter`]
//!   and parsed back with [`gamut_icc::IccProfile::parse`]: a v2 sampled-TRC sRGB, a v4
//!   parametric-TRC Display P3, two camera (`scnr`) profiles — one matrix/TRC over an XYZ PCS, one
//!   `lutAToB` over a Lab PCS — a v4 CMYK printer with a table per rendering intent, and one
//!   truncated profile for the parser's error path. No `.icc` file is committed.
//! - [`scene`] — the pixels every scenario feeds in: a patch chart, and the raw sensor values a
//!   camera would record for it.
//! - [`formats`] — the same pixels embedded in PNG (`iCCP`) and JPEG (APP2 `ICC_PROFILE`) with
//!   `gamut-png`/`gamut-jpeg`, and the profile extracted back out of each container.
//! - [`panels`] — the per-panel computations: the TRC evaluator, the tag/matrix inspector, the
//!   CLUT slice, the full image pipeline with intent/BPC toggles, and the xy gamut hull.
//! - `conformance` (feature `oracle`, native only) — the battery cross-checked against Little-CMS,
//!   reduced to the max-ΔE₀₀ badge.
//!
//! The binary (`src/main.rs`, feature `gui`) draws those panels with egui/eframe, natively or in
//! a browser canvas. **Every displayed colour is converted to sRGB by `gamut-cmm` itself** and then
//! handed to the canvas untagged, so neither the OS nor the browser colour-manages it a second
//! time. The only exceptions are values that are data rather than colour: ink-coverage plates and
//! the one-channel `gamt` table, which are drawn as grey levels.

#[cfg(feature = "oracle")]
pub mod conformance;
pub mod formats;
pub mod panels;
pub mod profiles;
pub mod scene;
