//! `gamut-png` — a research-grade, space-efficient **PNG codec** (PNG 3rd edition; W3C).
//!
//! PNG is a lossless raster format: an 8-byte signature followed by typed chunks (IHDR, optional
//! palette/colour/metadata chunks, IDAT image data, IEND). The image data is scanline-filtered and
//! then DEFLATE-compressed. The encoder builds on [`gamut_deflate`] for the compression and aims
//! for output sizes on par with the best PNG encoders, trading encode time for size at higher
//! levels. The decoder ([`PngDecoder`], issue #249) covers the full still-image spec — every
//! colour type and bit depth, Adam7 interlacing, all filters — behind hostile-input limits, and
//! surfaces ancillary metadata (EXIF/ICC/XMP/text) as raw payloads. Animation (APNG) is out of
//! scope. Correctness in both directions is proven differentially against a vendored libpng.
//!
//! # Reading metadata without the pixels
//!
//! [`metadata`] walks the chunk stream and returns a [`PngMetadata`] — the EXIF/ICC/XMP/text
//! payloads plus the parsed colour chunks (`cICP`, `sRGB`, `gAMA`, `cHRM`) — skipping IDAT by
//! length, so no pixel data is read or inflated. It is the counterpart of `gamut_jpeg::metadata`
//! and `gamut_webp::metadata`, and what a colour-space probe should call.
//! [`PngDecoder::metadata`] is the same walk with a configurable inflation budget.
//!
//! # Pluggable IDAT backends
//!
//! The PNG codestream is the concatenated-IDAT **zlib stream**, and it is where PNG spends its
//! time. The [`backend`] module opens that one seam: push an [`IdatDeflater`] or [`IdatInflater`]
//! to route it through a hardware DEFLATE engine (Intel QAT/IAA, IBM zEDC, POWER nx-gzip) or a
//! faster software library (zlib-ng, libdeflate), with [`AbiDeflater`] / [`AbiInflater`] adapting a
//! [`gamut_codec_abi`] backend. The built-ins ([`gamut_deflate`] and `miniz_oxide`) stay the
//! implicit tail, so an encoder or decoder with nothing pushed behaves exactly as before.
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

mod abi;
mod adam7;
mod ancillary;
pub mod backend;
mod chunk;
mod color;
mod crc32;
mod decoded;
mod decoder;
mod deconstruct;
mod encoder;
mod filter;
mod ihdr;
mod inflate;
mod pack;
mod palette;
mod reduce;
/// The encoder's pipeline stages, re-exported for the out-of-tree benchmark driver (issue #224).
/// Not part of the stable API; see `docs/benchmarking.md`.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod stages;

pub use abi::{AbiDeflater, AbiInflater, CODEC_ID_ZLIB, PIXEL_FORMAT_FILTERED_BYTES};
pub use ancillary::{PhysicalUnit, SrgbIntent};
pub use backend::{IdatDeflater, IdatInflater, IdatInfo};
pub use color::ColorType;
pub use decoded::{
    Chromaticities, Cicp, DecodedPng, IccProfile, PngHeader, PngImage, PngMetadata, TextChunk,
};
pub use decoder::{PngDecoder, TransparencyKey, metadata};
pub use deconstruct::{
    ChunkStats, DEFAULT_MAX_CHUNKS, DeconstructLimits, FilterHistogram, FilterScan, PassStats,
    PngReport, Segment, SegmentKind, SkippedFilterScan, deconstruct, deconstruct_with_limits,
};
pub use encoder::PngEncoder;
pub use filter::{FilterStrategy, FilterType};
/// The DEFLATE compression level, accepted by [`PngEncoder::with_compression`].
pub use gamut_deflate::Level;
pub use palette::PngPalette;
