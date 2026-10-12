//! Color primitives for the gamut codecs: coded-plane bit depth and chroma subsampling, the CICP
//! code points carried in nclx / AV1 sequence headers, and planar buffers. Interleaved pixel
//! layouts are not modelled here — that is the [`Pixel`](gamut_core::Pixel) vocabulary in
//! `gamut-core`.
//!
//! The M0 AVIF encoder uses only a narrow slice — 8-bit RGB in, mapped to identity (`mc = 0`)
//! 4:4:4 planes. The enums here intentionally model the wider spec surface (more formats, bit
//! depths, subsamplings, and CICP code points) so later milestones (M2 pixel formats, M4 HDR;
//! see `gamut-avif/STATUS.md`) extend without reshaping the types.
//!
//! On top of that metadata layer, the [`transfer`], [`oklab`], [`lab`], [`matrix`],
//! [`gamut_map`], [`cct`], and [`profile`] modules add `f64` colour science — encoder-exact EOTFs, OKLab
//! transforms with per-gamut matrices (derived from chromaticities via Bradford adaptation),
//! CIELab/LCh/xyY colorimetry with the ICC PCS fixed-point encodings and the ΔE\*ab / CIEDE2000
//! colour-difference metrics, correlated colour temperature by Robertson's method, gamut clamping,
//! and source-profile bundles over the CICP axes; the
//! [`linalg`] module exports the shared 3×3 helpers underneath them. This math is **Tier-1**
//! (correctness only): it uses `std` `f64`, so it is not bit-reproducible across platforms — see
//! `references/color/README.md`.
//!
//! # API layout
//!
//! Every module is public, so the full surface — including the colour-science long tail (the `M1` /
//! `M2` matrices, the `*_standard` transfer-curve variants, the matrix derivations) — is reachable
//! and grouped under its module. For convenience the crate root additionally re-exports the items
//! most consumers name directly: the CICP enums, [`BitDepth`] / [`ChromaSubsampling`],
//! [`Planar8`] / [`Yuv420`] / [`YcbcrMatrix`] / [`RgbToYcbcr`], the [`clip_pixel`] /
//! [`rgb_to_ycbcr`] helpers, the colour-science entry types [`Gamut`] and [`SourceProfile`] /
//! [`SourceTransfer`], and the headline colour-difference metrics [`delta_e_76`] /
//! [`delta_e_2000`].
//!
//! # Implemented vs. modeled
//!
//! Many enum variants model the full spec surface but are **not yet wired into an encode path** —
//! they exist so later milestones extend without reshaping the types. As of this release:
//!
//! - **Implemented:** 8-bit ([`BitDepth::Eight`]) RGB → 4:4:4, 4:2:2 or 4:2:0 planes,
//!   either identity ([`MatrixCoefficients::Identity`]) or through a CICP luma–chroma matrix
//!   ([`Planar8`]); the CICP code-point tables; BT.601 YCbCr 4:2:0 ([`ycbcr`]); **the H.273
//!   matrixing and de-matrixing at every modeled bit depth for [`MatrixCoefficients::Bt709`] /
//!   `Bt601` / `Bt470Bg` / `Bt2020Ncl` in both ranges ([`RgbToYcbcr`] / [`YcbcrMatrix`])**; and the
//!   `f64` colour science ([`transfer`], [`oklab`], [`lab`], [`xyb`], [`matrix`], [`gamut_map`],
//!   [`cct`], [`profile`]) for the sRGB, Display P3, Adobe RGB, BT.2020 and ProPhoto gamuts.
//! - **Modeled but deferred:** 10/12-bit *plane* wiring ([`BitDepth::Ten`] / [`BitDepth::Twelve`] —
//!   both H.273 directions ship at these depths; what is missing is the AV1 encode path and a
//!   [`Planar8`] geometry to carry them. Distinct from [`BitDepth::Sixteen`], which is outside the
//!   AV1 profile set entirely and exists for the 16-bit still-image pipelines that share these
//!   types); [`ChromaSubsampling::Cs400`] (monochrome) as an encode path;
//!   [`MatrixCoefficients::YCgCo`], the one modeled matrix with neither
//!   direction (it is a lifting transform, not a `Kr`/`Kb` matrix). These land with the
//!   milestones tracked in `gamut-avif/STATUS.md`.
//!
//! `Bt709`, `Bt601`, `Bt2020_10` and `Bt2020_12` — one curve in ITU-T H.273 Table 3 — dispatch in both directions to
//! [`bt709_eotf`](transfer::bt709_eotf) / [`bt709_oetf`](transfer::bt709_oetf), and `Hlg` to
//! [`hlg_inverse_oetf`](transfer::hlg_inverse_oetf) / [`hlg_oetf`](transfer::hlg_oetf) (the
//! scene-referred Table 3 curve; the display-dependent BT.2100 OOTF is not modelled).
//! [`oetf_for`](transfer::oetf_for) returns `None` for `Pq`, where
//! [`eotf_for`](transfer::eotf_for) returns `Some`: that arm is the tone-mapping
//! [`bt2020_pq_to_sdr`](transfer::bt2020_pq_to_sdr), which is not invertible. The standards-pure
//! pair [`pq_eotf`](transfer::pq_eotf) / [`pq_oetf`](transfer::pq_oetf) is exact in both
//! directions, and [`standard_eotf_for`](transfer::standard_eotf_for) /
//! [`standard_oetf_for`](transfer::standard_oetf_for) are the same dispatch with that pair
//! (normalized to the 10 000 cd/m² peak) in the PQ arm. [`SourceTransfer::eotf`](profile::SourceTransfer::eotf) — the dispatch over the
//! gamuts with no CICP transfer code point — likewise has no inverse yet.
// `deny`, not `forbid`, because this crate is on an encode hot path: a measured win may take
// the exception (AGENTS.md, `## Conventions`). None does today — this is 100% safe Rust.
#![deny(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod cct;
pub mod cicp;
pub mod format;
pub mod gamut_map;
// Executable laws for this crate's constructors, shared by its property tests and the
// out-of-tree fuzz tier (`test-support`). Not part of the stable API; see `docs/testing.md`.
// A plain comment, not `///`, so rustdoc resolves the module's own `//!` links in its scope.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
pub mod lab;
pub mod linalg;
pub mod matrix;
pub mod oklab;
pub mod pixel;
pub mod planar;
pub mod planar16;
pub mod profile;
pub mod transfer;
pub mod xyb;
pub mod ycbcr;

pub use cct::cct_from_xy;
pub use cicp::{ColorRange, ColourPrimaries, MatrixCoefficients, TransferCharacteristics};
pub use format::{BitDepth, ChromaSubsampling};
pub use lab::{delta_e_76, delta_e_2000};
pub use oklab::Gamut;
pub use pixel::{clip_pixel, clip_pixel8};
pub use planar::Planar8;
pub use planar16::Planar16;
pub use profile::{SourceProfile, SourceTransfer};
pub use ycbcr::{RgbToYcbcr, YcbcrMatrix, Yuv420, rgb_to_ycbcr, ycbcr_to_rgb};
