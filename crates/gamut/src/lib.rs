//! `gamut` — the umbrella crate for a collection of space-efficient image encoding libraries.
//!
//! This crate re-exports the high-level APIs of the format-specific crates behind Cargo
//! features, so a consumer compiles only the codecs they need:
//!
//! ```sh
//! cargo add gamut -F avif,jxl
//! ```
//!
//! [`core`] (re-exported from [`gamut-core`](https://crates.io/crates/gamut-core)) is always
//! available and provides the shared [`core::EncodeImage`] / [`core::DecodeImage`] traits, the
//! branded [`core::ImageRef`] / [`core::ImageBuf`] buffers, and the common [`core::Error`] type.
//! Each format module appears only when its feature is enabled.
//!
//! The `primitives` feature additionally re-exports the shared building blocks the codecs are
//! made of — `color` (pixel formats / CICP), `dsp` (transforms), and `bitstream` (bit writer
//! + AV1 symbol coder) — for tooling and sandbox use such as the `gamut` CLI.
//!
//! The `metadata` feature re-exports the shared image-metadata primitives (issue #34) — the
//! TIFF/IFD container core (`ifd`) plus, as they land, the per-format crates (`exif`/`xmp`/`icc`/
//! `iptc`) and the `metadata` facade that unifies them — for tooling and for the container crates
//! to consume.
//!
//! The `cmm` feature re-exports the ICC colour management module (`cmm`, epic #323) — the
//! transform engine that builds and applies colour transforms from the profiles `gamut-icc`
//! parses — for colour-managed decode/encode pipelines and tooling.
//!
//! The `isobmff` feature re-exports the ISOBMFF/HEIF still-image container primitive (`isobmff`) —
//! the box tree shared by the AVIF and HEIC codecs — so tooling can read and write containers
//! directly, independently of the codecs that fill them.
//!
//! The `codec-abi` feature re-exports the shared codestream-backend seam (`codec_abi`) — the
//! `repr(C)` vtables and their object-safe Rust twin traits by which a foreign (C/FFI) or alternate
//! codestream backend plugs into a format crate — for anyone implementing or adapting a backend.
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them. The
// README's example encodes AVIF, so it compiles only with that feature.
#[cfg(all(doctest, feature = "avif"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// The version of this `gamut` library crate, taken from its `Cargo.toml` at compile time.
///
/// Exposed so tooling (e.g. the `gamut` CLI's `-V` output) can report the resolved library
/// version without hardcoding it. Because each workspace crate is versioned independently, this
/// can differ from the version of the binary or consumer that links it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "av1")]
pub use gamut_av1 as av1;
#[cfg(feature = "av2")]
pub use gamut_av2 as av2;
#[cfg(feature = "avif")]
pub use gamut_avif as avif;
#[cfg(feature = "primitives")]
pub use gamut_bitstream as bitstream;
#[cfg(feature = "cmm")]
pub use gamut_cmm as cmm;
#[cfg(feature = "codec-abi")]
pub use gamut_codec_abi as codec_abi;
#[cfg(feature = "primitives")]
pub use gamut_color as color;
pub use gamut_core as core;
#[cfg(feature = "dng")]
pub use gamut_dng as dng;
#[cfg(feature = "primitives")]
pub use gamut_dsp as dsp;
#[cfg(feature = "metadata")]
pub use gamut_exif as exif;
#[cfg(feature = "heic")]
pub use gamut_heic as heic;
#[cfg(feature = "metadata")]
pub use gamut_icc as icc;
#[cfg(feature = "metadata")]
pub use gamut_ifd as ifd;
#[cfg(feature = "metadata")]
pub use gamut_iptc as iptc;
#[cfg(feature = "isobmff")]
pub use gamut_isobmff as isobmff;
#[cfg(feature = "jpeg")]
pub use gamut_jpeg as jpeg;
#[cfg(feature = "jxl")]
pub use gamut_jxl as jxl;
#[cfg(feature = "metadata")]
pub use gamut_metadata as metadata;
#[cfg(feature = "png")]
pub use gamut_png as png;
#[cfg(feature = "tiff")]
pub use gamut_tiff as tiff;
#[cfg(feature = "tonemap")]
pub use gamut_tonemap as tonemap;
#[cfg(feature = "vvc")]
pub use gamut_vvc as vvc;
#[cfg(feature = "webp")]
pub use gamut_webp as webp;
#[cfg(feature = "metadata")]
pub use gamut_xmp as xmp;
