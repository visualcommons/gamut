//! Shared digital signal processing kernels for the gamut codecs.
//!
//! The crate is one module per spec family, plus the shared integer vocabulary:
//! - [`av1`] — the AV1 §7.13.2 transform kernels: the 1-D DCT / ADST / identity kernels and the
//!   lossless 4×4 Walsh–Hadamard block pair,
//! - [`jpeg`] — the JPEG-1 (ITU-T T.81 | ISO/IEC 10918-1) §A.3.3 8×8 forward and inverse DCT
//!   primitives,
//! - [`math`] — the small cross-codec integer arithmetic primitives: the AV1 §4.7 rounding and
//!   clamp operations and the forward-quantize rounding shared by the AV1 and VP8 encoders.
//!
//! Nothing lives at the crate root, so each spec family is its own sibling module and future ones
//! (JPEG XL, AV2, …) land the same way without ever colliding with an existing name.
//!
//! # Contract
//!
//! Every function is pure, total, deterministic math on caller-provided values — no
//! allocation, no dependencies, no `unsafe`, and no `Result`s: nothing here is
//! data-dependent fallible. Semantic preconditions on configuration parameters (`n`, `r`,
//! `bits`, `den` — encoder configuration, never untrusted data) are asserts documented
//! under `# Panics`; arithmetic headroom limits are documented per function and guarded by
//! Rust's debug overflow checks.
//!
//! ```
//! use gamut_dsp::av1::{forward_wht4x4, inverse_wht4x4};
//! use gamut_dsp::math::round_div_nearest;
//!
//! // The lossless 4×4 Walsh–Hadamard pair round-trips exactly.
//! let residual = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
//! let coeffs = forward_wht4x4(&residual);
//! assert_eq!(inverse_wht4x4(&coeffs), residual);
//!
//! // The shared forward-quantize rounding divides to the nearest level, ties away from zero.
//! assert_eq!(round_div_nearest(-10, 4), -3);
//! ```
// `deny`, not `forbid`, because this crate is on an encode hot path: a measured win may take
// the exception (AGENTS.md, `## Conventions`). None does today — this is 100% safe Rust.
#![deny(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(test)]
mod testrng;

pub mod av1;
pub mod jpeg;
pub mod math;
