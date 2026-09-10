//! `gamut-jxl` — a JPEG XL (JXL) image **encoder and decoder**.
//!
//! Unlike the clean-slate codecs elsewhere in the workspace, gamut-jxl is a thin, safe layer over
//! the JPEG XL reference implementations:
//!
//! - the **encoder** wraps the ISO/IEC 18181 reference implementation, libjxl 0.12.0, statically
//!   linked through [`gamut-jxl-sys`](https://crates.io/crates/gamut-jxl-sys) — the Rust ecosystem
//!   has no reference-quality JPEG XL *encoder*, so wrapping libjxl is the sole spec-faithful path;
//! - the **decoder** wraps the pure-Rust [`jxl`](https://crates.io/crates/jxl) crate (jxl-rs, the
//!   libjxl org's Rust decoder), so decoding needs no C toolchain and works on every `wasm32`
//!   target.
//!
//! The departure from the workspace's "pure Rust" norm — and its licensing and build implications —
//! is a maintainer-confirmed decision recorded in the crate `README.md`.
//!
//! # Encoding
//!
//! Build a [`JxlEncoder`] — [`JxlEncoder::lossless`] (the default) or [`JxlEncoder::lossy`] with a
//! validated [`Distance`] — tune it with the chainable builders ([`JxlEncoder::with_effort`],
//! [`JxlEncoder::with_modular`] to pin the VarDCT/Modular coding tool,
//! [`JxlEncoder::with_container`], [`JxlEncoder::with_color`] for sRGB/linear/PQ/HLG/ICC
//! signalling, [`JxlEncoder::with_orientation`], and [`JxlEncoder::with_exif`] /
//! [`JxlEncoder::with_xmp`] for container metadata boxes), then encode any of the eight supported
//! pixel layouts (8/16-bit grayscale, gray+alpha, RGB, RGBA) through the
//! [`EncodeImage`](gamut_core::EncodeImage) trait. [`JxlEncoder::recompress_jpeg`] losslessly
//! re-packs an existing JPEG so the original is reconstructible bit-for-bit (jbrd).
//! The codestream is produced by a [`JxlCodestreamEncoder`] backend; the built-in libjxl wrapper is
//! the implicit last one (see [Backends](#backends)).
//!
//! # Decoding
//!
//! Build a [`JxlDecoder`] and decode a JPEG XL byte stream — bare codestream or ISO BMFF container —
//! into any of the same eight pixel layouts through the [`DecodeImage`](gamut_core::DecodeImage)
//! trait. The decoder converts internally where the request and the stream differ (grayscale →
//! RGB, opaque-alpha padding, alpha dropping) and refuses lossy guesses such as reading a colour
//! image back as grayscale. The codestream is decoded by a [`JxlCodestreamDecoder`] backend; the
//! built-in jxl-rs wrapper is the implicit last one (see [Backends](#backends)).
//!
//! A truncated stream is an [`Error::InvalidInput`](gamut_core::Error::InvalidInput) on that path.
//! [`DecodePartialImage`] is the opt-in alternative: it returns the best-effort image plus a
//! [`JxlPartialReport`] carrying the completeness flag, for a partly-downloaded or damaged file.
//! Read its documentation before relying on the pixels — "best effort" can legitimately mean a
//! blank buffer.
//!
//! # Example: lossless round-trip
//!
//! With the default features (`encode` + `decode`), a lossless stream decodes back bit-exact. The
//! example needs both codec halves, so it is compiled and run only where they are present — the
//! `cargo test --doc` environment on a non-`wasm32` host:
//!
//! ```
//! use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
//! use gamut_jxl::{JxlDecoder, JxlEncoder};
//!
//! // A 4×4 8-bit RGB test image.
//! let dims = Dimensions { width: 4, height: 4 };
//! let pixels: Vec<u8> = (0..4 * 4 * 3).map(|i| i as u8).collect();
//! let image = ImageRef::<Rgb8>::new(&pixels, dims)?;
//!
//! // Lossless encode, then decode straight back to the same layout.
//! let stream = JxlEncoder::lossless().encode_to_vec(image)?;
//! let decoded: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&stream)?;
//!
//! // Lossless output is bit-exact to the input.
//! assert_eq!(decoded.dimensions(), dims);
//! assert_eq!(decoded.as_samples(), pixels.as_slice());
//! # Ok::<(), gamut_core::Error>(())
//! ```
//!
//! # Backends
//!
//! Both directions are **registries of codestream backends** (issue #276), not a fixed
//! implementation. The seam is the bare JPEG XL codestream (signature `FF 0A`):
//! [`JxlEncoder::push_backend`] and [`JxlDecoder::push_backend`] insert a platform or alternate
//! implementation, tried in push order, and the crate's own wrappers are the implicit **tails**.
//! [`crate::backend`] documents the fallback contract; [`crate::abi`] adapts a
//! [`gamut_codec_abi`] backend (including a C one) onto the same door.
//!
//! ## What the `encode` / `decode` features mean
//!
//! They select **whether the built-in tail is compiled in** — they do *not* enable or disable the
//! direction:
//!
//! - `encode` (default) includes the libjxl encode tail, where the target supports it. Without it,
//!   [`JxlEncoder`] still exists and still encodes — through whatever backend was pushed. With
//!   neither, encoding returns [`Error::Unsupported`](gamut_core::Error::Unsupported).
//! - `decode` (default) includes the jxl-rs decode tail, and additionally provides the header-only
//!   accessors ([`JxlDecoder::info`], [`JxlDecoder::embedded_icc_profile`], [`JxlInfo`]), the
//!   metadata read-back ([`JxlDecoder::metadata`] → [`JxlMetadata`]: the container's `Exif` /
//!   `xml ` boxes, located by this crate's own box walk, plus the codestream ICC profile) and the
//!   best-effort [`DecodePartialImage`] surface, all of which are always answered by the built-in
//!   parser. Without it, [`JxlDecoder`] decodes through a pushed backend, or returns
//!   [`Error::Unsupported`](gamut_core::Error::Unsupported).
//!
//! This is why the encode direction works on `wasm32-unknown-unknown` despite libjxl being
//! unbuildable there: push a backend and the tail's absence stops mattering.
//!
//! ## The `metadata` feature
//!
//! Off by default. It adds the `gamut-metadata` facade's typed models over the raw surface above:
//! [`JxlMetadata::blocks`] hands the located payloads over as `MetadataBlock`s and
//! [`JxlMetadata::metadata`] parses them into a unified `Metadata`, while
//! [`JxlEncoder::with_metadata`] / [`JxlEncoder::with_encoded_metadata`] embed one, routing EXIF
//! and XMP to the container boxes and the ICC profile to [`ColorSpec::Icc`]. The dependency
//! direction stays `gamut-jxl → gamut-metadata`.
//!
//! ## Deferred: container ownership
//!
//! Container-dependent features — ISO BMFF output, `Exif`/`xml ` boxes, and `jbrd` JPEG
//! recompression — are written *by libjxl* today, so they are pinned to the built-in path by a
//! host-side veto and never reach a backend. Moving that box/`jbrd` writing into gamut-jxl proper,
//! so a pushed backend's codestream can be wrapped in a gamut-written container, is a recorded
//! follow-up (see `STATUS.md`), not part of this crate today.
//!
//! # Safety and portability
//!
//! The crate is `#![deny(unsafe_code)]`; all `unsafe` is confined to the single `ffi` module that
//! drives libjxl (hence `deny` rather than `forbid`). The decoder is 100% safe Rust and available
//! on every target.
//!
//! The encoder is compiled in for
//! `all(feature = "encode", any(not(target_arch = "wasm32"), target_os = "emscripten"))` — that
//! is, everywhere except `wasm32` targets that emscripten does not cover:
//!
//! - **`wasm32-unknown-emscripten`** gets the full encoder: libjxl officially supports wasm via
//!   emscripten, and `gamut-jxl-sys` builds it with the emsdk toolchain (`emcc` on `PATH`).
//! - **`wasm32-unknown-unknown`** (the wasm-bindgen/browser target) is decode-only, permanently by
//!   toolchain boundary rather than by workaround: no C/C++ compiler emits archives for that ABI,
//!   so no build configuration could link libjxl there. A pure-Rust JPEG XL encoder is the only
//!   thing that could ever change this (jxl-rs ships none).
#![deny(unsafe_code)]

pub mod abi;
pub mod backend;
mod config;
#[cfg(feature = "decode")]
mod convert;
mod decoder;
mod encoder;
// The error-mapping module carries an encoder half (libjxl statuses) and a decoder half (jxl-rs
// errors), each independently feature-gated inside the module; it is present whenever either codec
// half is compiled.
#[cfg(any(
    all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ),
    feature = "decode"
))]
mod error;
#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
mod ffi;
#[cfg(feature = "decode")]
mod jxlrs;

pub use abi::{AbiDecodeBackend, AbiEncodeBackend, JXL_CODEC_ID};
pub use backend::{
    JxlCodestreamDecoder, JxlCodestreamEncoder, JxlDecoded, JxlEncodeRequest, JxlFraming,
    JxlImageRef, JxlOwnedSamples, JxlSamples, JxlStreamInfo,
};
pub use config::{ColorSpec, Container, Distance, Effort, ModularMode, Orientation};
#[cfg(feature = "decode")]
pub use decoder::{DecodePartialImage, JxlInfo, JxlPartialReport, JxlRender};
pub use decoder::{JxlDecoder, JxlMetadata};
pub use encoder::JxlEncoder;
// The facade types named in the `metadata`-feature signatures, so a caller can spell
// `JxlMetadata::metadata` / `JxlEncoder::with_metadata` without a direct dependency.
#[cfg(feature = "metadata")]
pub use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock};
