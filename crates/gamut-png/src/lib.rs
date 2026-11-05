//! `gamut-png` — a space-efficient **PNG codec** (PNG 3rd edition; W3C).
//!
//! PNG is a lossless raster format: an 8-byte signature followed by typed chunks (IHDR, optional
//! palette/colour/metadata chunks, IDAT image data, IEND). The image data is scanline-filtered and
//! then DEFLATE-compressed. The encoder builds on [`gamut_deflate`] for the compression and aims
//! for output sizes on par with the best PNG encoders, trading encode time for size at higher
//! levels. The decoder ([`PngDecoder`], issue #249) covers the full still-image spec — every
//! colour type and bit depth, Adam7 interlacing, all filters — behind hostile-input limits, and
//! surfaces ancillary metadata (EXIF/ICC/XMP/text, and the C2PA manifest store) as raw payloads.
//! Animation (APNG) is out of scope. Correctness in both directions is proven differentially
//! against a vendored libpng.
//!
//! # Reading metadata without the pixels
//!
//! [`metadata`] walks the chunk stream and returns a [`PngMetadata`] — the EXIF/ICC/XMP/text
//! payloads plus the parsed colour chunks (`cICP`, `sRGB`, `gAMA`, `cHRM`) — skipping IDAT by
//! length, so no pixel data is read or inflated. It is the counterpart of `gamut_jpeg::JpegDecoder::metadata`
//! and `gamut_webp::WebpDecoder::metadata`, and what a colour-space probe should call.
//! [`PngDecoder::metadata`] is the same walk with a configurable inflation budget.
//!
//! With the optional **`metadata`** Cargo feature the EXIF/XMP/ICC/C2PA payloads are wired to the
//! `gamut-metadata` facade's typed models through its `TypedMetadata` trait, implemented by
//! [`PngMetadata`] (`blocks` / `metadata`; a full decode holds one as [`DecodedPng::metadata`]),
//! and its `MetadataSink` trait, implemented by [`PngEncoder`]: `with_metadata` /
//! `with_encoded_metadata` embed a unified `Metadata`, routing each carrier to the raw setters.
//! The chunk-for-chunk carry of a [`PngMetadata`] is [`PngEncoder::with_png_metadata`].
//!
//! # C2PA manifest store
//!
//! A C2PA manifest store travels in the `caBX` chunk (C2PA 2.4 §A.3.2: ancillary, private,
//! **unsafe to copy**), raw and uncompressed. gamut locates, bounds, carries and reserves it and
//! never judges it: the store is opaque bytes here, and validation is `c2pa-rs`'s. On read it is
//! [`PngMetadata::c2pa`] (also [`DecodedPng::metadata`]), the first CRC-valid `caBX` before the first
//! `IDAT`, under the same metadata budget as every other ancillary payload. On write, [`PngEncoder::with_c2pa`] embeds a
//! store computed for this file and [`PngEncoder::with_c2pa_reserved`] reserves its place, as the
//! last chunk before `IDAT`; [`PngEncoder::encode_with_report`] and [`PngReport::c2pa`] name the
//! chunk's **whole** span — length, type, payload and CRC — which is what a `c2pa.hash.data`
//! assertion excludes (C2PA §18.5.4). [`fill_c2pa`] then writes the finished store into that span in
//! place, rewriting only the payload and the chunk CRC, so the file the signer hashed is the file
//! it signed. For a reader rather than a signer, [`c2pa_summary`] reports the same store beside
//! every other `caBX` chunk the file carries — each with the reason it is not the store — and
//! renders the lines, with [`C2PA_NOT_VALIDATED`] inline, that `gamut inspect` prints.
//!
//! # Pluggable IDAT backends
//!
//! The PNG codestream is the concatenated-IDAT **zlib stream**, and it is where PNG spends its
//! time. One seam opens it: push a [`ZlibDeflater`] or [`ZlibInflater`] with
//! [`PngEncoder::push_backend`] / [`PngDecoder::push_backend`] to route it through a hardware
//! DEFLATE engine (Intel QAT/IAA, IBM zEDC, POWER nx-gzip) or a faster software library (zlib-ng,
//! libdeflate), with [`AbiZlibDeflater`] / [`AbiZlibInflater`] adapting a [`gamut_codec_abi`]
//! backend under [`ZLIB_CODEC_ID`]. The built-ins ([`gamut_deflate`] and `miniz_oxide`) stay the
//! implicit tail, so an encoder or decoder with nothing pushed behaves exactly as before.
//!
//! Backends are tried in push order. [`ZlibInflater::supports`] returning `false` is the only
//! fall-through; a backend that accepts and then fails ends the job with its error. The decoder
//! re-checks every inflater's output length against its own budget, so an untrusted backend
//! cannot turn a zlib bomb into an out-of-memory.
//!
//! # Example
//!
//! ```
//! use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
//! use gamut_png::{PngDecoder, PngEncoder};
//!
//! let (w, h) = (2, 2);
//! let rgb = vec![7u8; (w * h * 3) as usize];
//! let image = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
//! let mut png = Vec::new();
//! PngEncoder::new().encode_image(image, &mut png).unwrap();
//! assert_eq!(&png[1..4], b"PNG");
//!
//! let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
//! assert_eq!(decoded.as_samples(), rgb);
//! ```
// `deny`, not `forbid`, because this crate is on an encode hot path: a measured win may take
// the exception (AGENTS.md, `## Conventions`). None does today — this is 100% safe Rust.
#![deny(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod abi;
mod adam7;
mod ancillary;
mod backend;
mod carry;
mod chunk;
mod color;
mod crc32;
mod decoded;
mod decoder;
mod deconstruct;
mod effort;
mod encoder;
mod filter;
mod ihdr;
mod inflate;
mod pack;
mod palette;
mod present;
mod reduce;
/// The encoder's pipeline stages, re-exported for the out-of-tree benchmark driver (issue #224).
/// Not part of the stable API; see `docs/benchmarking.md`.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod stages;
mod text;
#[cfg(feature = "metadata")]
mod typed;
mod walk;
mod written;

pub use abi::{AbiZlibDeflater, AbiZlibInflater, PIXEL_FORMAT_FILTERED_BYTES, ZLIB_CODEC_ID};
pub use ancillary::{PhysicalUnit, SrgbIntent};
pub use backend::{IdatInfo, ZlibDeflater, ZlibInflater};
pub use carry::MetadataNotice;
pub use chunk::{
    C2PA_NOT_VALIDATED, C2paSpan, C2paSummary, C2paUnreadChunk, C2paUnreadReason, c2pa_summary,
    fill_c2pa,
};
pub use color::ColorType;
pub use decoded::{
    Background, Chromaticities, Cicp, DecodedPng, IccProfile, ModificationTime, PhysicalDimensions,
    PngHeader, PngImage, PngMetadata, TextChunk, TextChunkKind,
};
pub use decoder::{PngDecoder, TransparencyKey};
pub use deconstruct::{
    ChunkStats, DEFAULT_MAX_CHUNKS, DeconstructLimits, FilterHistogram, FilterScan, PassStats,
    PngReport, Segment, SegmentKind, SkippedFilterScan, deconstruct, deconstruct_with_limits,
};
pub use effort::Effort;
pub use encoder::{PngEncodeReport, PngEncoder};
pub use filter::{FilterStrategy, FilterType};
/// The workspace's shared vocabulary, accepted by [`PngDecoder::with_limits`],
/// [`PngDecoder::with_convert_policy`] and [`PngEncoder::encode_indexed8`] (and returned in
/// [`DecodedPng::palette`]), re-exported so a caller needs no direct `gamut-core`
/// dependency to spell them.
pub use gamut_core::{DecodeLimits, Palette, convert::ConvertPolicy};
/// The DEFLATE compression level, accepted by [`PngEncoder::with_compression`].
pub use gamut_deflate::Level;
// The facade types named in the `metadata`-feature signatures, so a caller can spell
// `PngMetadata::metadata` / `PngEncoder::with_metadata` without a direct dependency.
#[cfg(feature = "metadata")]
pub use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, MetadataSink, TypedMetadata};
pub use palette::PaletteOrder;
pub use walk::metadata;
