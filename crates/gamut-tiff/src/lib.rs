//! `gamut-tiff` — TIFF 6.0 (Tagged Image File Format) image encoder and decoder.
//!
//! TIFF is a *natively still-image* format: its Image File Directory (IFD) / tag structure **is**
//! the container, so this crate needs neither
//! [`gamut_isobmff`](https://crates.io/crates/gamut-isobmff) (AVIF/HEIC) nor
//! [`gamut_riff`](https://crates.io/crates/gamut-riff) (WebP). That IFD container core — the
//! byte-order header, field types/values, the IFD chain, and the offset-driven read/write spine —
//! is the shared [`gamut_ifd`](https://crates.io/crates/gamut-ifd) primitive (also the basis for
//! EXIF); this crate adds the codec on top and re-exports the structural types from its root so its
//! public API is unchanged. It further layers on the shared primitives: [`gamut_core`] (traits /
//! errors / typed pixel formats), [`gamut_bitstream`] (LZW and CCITT bit coding), and
//! [`gamut_deflate`](https://crates.io/crates/gamut-deflate) for Adobe Deflate (its encoder, and
//! the bounded inflate behind its `inflate` feature).
//! The differencing predictor is TIFF-specific and lives in this crate;
//! the deferred colour-space work (YCbCr, CIE L\*a\*b\*) and JPEG-in-TIFF will bring back the
//! `gamut-color` and `gamut-dsp` edges additively when they land (see `STATUS.md`).
//!
//! The encoder and decoder are reachable through the umbrella crate's `tiff` feature. Everything
//! is implemented clean-slate from the TIFF 6.0 specification (`references/tiff/tiff6.pdf`,
//! Adobe/Aldus, Final — June 3 1992), Adobe Photoshop TIFF Technical Note 3 (Deflate), and the
//! BigTIFF extension (`references/tiff/bigtiff.html`) rather than wrapping libtiff.
//!
//! The current major's surface (built in issue #107, first stabilised in issue #187):
//! [`TiffEncoder`] writes 8- and 16-bit grayscale/RGB/RGBA, 8-bit CMYK, 1-bit bilevel, and 8-bit palette images (as strips or tiles,
//! single- or multi-page) — uncompressed, PackBits, LZW/Deflate (optionally with the
//! horizontal-differencing [`Predictor`]), or (for bilevel) Modified Huffman / Group 4 fax —
//! and [`TiffDecoder`] reads them all back. Encoding takes a typed [`gamut_core::ImageRef`] via
//! the per-format [`gamut_core::EncodeImage`] impls, and decoding returns a
//! [`gamut_core::ImageBuf`] via [`gamut_core::DecodeImage`]. Both the classic 32-bit container
//! and **BigTIFF** (magic `43`, 64-bit offsets, for files past 4 GiB) are written and read: opt
//! into BigTIFF with [`TiffEncoder::with_big_tiff`]; the decoder detects the variant from the
//! header. [`TiffDecoder::info`] reports a page's declared depth, sample format and layout from
//! tags alone, so a caller can pick a pixel type before paying for a decode — including for pages
//! this crate declines to decode. Sample depths are 1, 8 and 16 bits; the typed impls widen 8-bit
//! samples to 16-bit exactly (`×257`) and, once [`TiffDecoder::with_convert_policy`] permits the loss,
//! narrow 16-bit to 8-bit by the shared engine's rescaling, while signed,
//! IEEE-float and 32-bit samples are refused by name rather than reinterpreted (§19 float support
//! and its Predictor 3 remain deferred). The strict [`deconstruct`] walk additionally accounts
//! every input byte and flags unknown tags and codes for archival triage. Every lossless path is pinned pixel-exact in both
//! directions against libtiff; the deferred colour modes and compression schemes (YCbCr,
//! CIE L\*a\*b\*, JPEG-in-TIFF, …) land additively — see `STATUS.md` for the scope ledger.
//!
//! Embedded metadata — Exif (the 0th-IFD fields of [`IFD0_EXIF_TAGS`] in IFD 0 itself, plus the
//! `ExifIFD` and `GPSInfo` sub-IFDs) plus XMP / IPTC-IIM / ICC / C2PA payloads — is
//! [`TiffMetadata`], written by [`TiffEncoder::with_tiff_metadata`] and read by
//! [`TiffDecoder::metadata`]. With the optional **`metadata`** Cargo feature it is also wired to
//! the `gamut-metadata` facade's typed models: [`TiffMetadata`] implements the facade's
//! `TypedMetadata` trait, and `TiffEncoder::with_metadata` / `with_encoded_metadata` embed a
//! unified `Metadata` (the whole EXIF model but its thumbnail, which has no carrier here and is
//! refused, as is a 0th-IFD field IFD 0 holds as the encoder's own).
//!
//! TIFF codestreams are permanently backend-less: unlike the other format crates, this crate
//! adds **no** pluggable codestream backend (the IoC seam of #241), because TIFF's compression
//! schemes have no hardware acceleration — gamut's software implementation is always used. (A
//! format crate that does expose its codestream does so through the one shared
//! `gamut-codec-abi` seam: a typed backend trait, a codec-abi adapter, and a push-order registry
//! with the crate's own implementation as the tail.) See `STATUS.md`.
//!
//! ```
//! use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
//! use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};
//!
//! let dims = Dimensions::new(2, 2)?;
//! let pixels = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
//! let image = ImageRef::<Rgb8>::new(&pixels, dims)?;
//!
//! let mut tiff = Vec::new();
//! TiffEncoder::new()
//!     .with_compression(Compression::PackBits)
//!     .encode_image(image, &mut tiff)?;
//!
//! let decoded: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff)?;
//! assert_eq!(decoded.as_samples(), &pixels[..]);
//! # Ok::<(), gamut_core::Error>(())
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

// Single canonical paths (the gamut-ifd v1 precedent): the implementation modules are private
// and the public surface is the crate-root re-export list below. `tags` is the one deliberate
// exception — a namespaced constants module, mirroring `gamut_ifd::tags`.
pub mod tags;

mod compression;
mod decoder;
mod deconstruct;
mod encoder;
mod ifd;
mod info;
mod metadata;
mod palette;
mod writer;

pub use decoder::TiffDecoder;
pub use deconstruct::deconstruct;
pub use encoder::{TiffEncodeReport, TiffEncoder};
// The core types this crate's own signatures take (`TiffDecoder::with_limits`,
// `TiffDecoder::with_convert_policy`), so a caller needs no direct gamut-core dependency.
pub use gamut_core::{DecodeLimits, convert::ConvertPolicy};
// The tag-value codes are gamut-ifd's, shared with gamut-dng; they are reachable from
// `TiffEncoder` and `TiffInfo`, so they are re-exported for the same reason as the IFD types below.
// Which codes a TIFF page may carry is this crate's own (`ifd`, `compression`).
pub use gamut_ifd::tags::{Compression, PhotometricInterpretation, Predictor, SampleFormat};
// The structural IFD core lives in gamut-ifd; re-export the types a gamut-tiff user can touch —
// the read/write spine plus every type reachable from this crate's own public items
// (`DeconstructReport` exposes `SegmentReport`, which exposes `Segment`/`SpanKind`/`DataLabel`/
// `Conflict`/`SharedSpan`, its ranges being `core::ops::Range<u64>`; `Value::Unknown` carries
// `UnknownValue`; and `Ifd`'s accessors return `SubIfd`) — so no direct gamut-ifd dependency is
// ever needed to name them. The deconstruct report itself (`DeconstructReport` and its
// `Anomaly`/`Severity`/`UnknownTag`/`UnknownFieldType` findings) is gamut-ifd's too, shared with
// gamut-dng.
pub use gamut_ifd::{
    Anomaly, ByteOrder, Conflict, DataLabel, DeconstructReport, Field, FieldType, Ifd, Segment,
    SegmentReport, Severity, SharedSpan, SpanKind, SubIfd, TiffFile, UnknownFieldType, UnknownTag,
    UnknownValue, Value, Variant, read, write,
};
// The C2PA exclusion set is gamut-ifd's — §A.3.6 and §18.5.5 are stated once, for this crate
// and `gamut-dng` alike — but it is reachable from `TiffEncodeReport` and [`c2pa_exclusions`], so
// it is re-exported here too and needs no direct gamut-ifd dependency. The non-validating report
// [`c2pa_summary`] returns is gamut-ifd's too, re-exported whole for the same reason (issue #530).
pub use gamut_ifd::{
    C2PA_NOT_VALIDATED, C2paExclusions, C2paStoreSummary, C2paSummary, C2paUnreadEntry,
    C2paUnreadReason,
};
// The facade types named in the `metadata`-feature signatures, so a caller can spell
// `TiffMetadata::metadata` / `TiffEncoder::with_metadata` without a direct dependency.
#[cfg(feature = "metadata")]
pub use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, TypedMetadata};
pub use info::TiffInfo;
pub use metadata::{IFD0_EXIF_TAGS, TiffMetadata, c2pa_exclusions, c2pa_summary};
pub use palette::Palette8;
pub use writer::{write_image, write_image_tiled, write_multipage};
