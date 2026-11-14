//! `gamut-dng` — a pure-Rust DNG (Adobe Digital Negative) raw-image **encoder and decoder**.
//!
//! DNG is Adobe's open raw format, built as a profile of **TIFF/EP**: an Image File Directory
//! (IFD) tree carries the camera's sensor samples plus the colour-calibration, geometry, and
//! metadata a raw processor needs to render them. Because the container *is* a TIFF, this crate
//! builds on the shared [`gamut_ifd`](https://crates.io/crates/gamut-ifd) IFD core (the same spine
//! `gamut-tiff` uses) and adds only the DNG-specific layers on top: the raw photometries
//! (`CFA` mosaic / `LinearRaw`), the colour/calibration model, the raw compression schemes, and
//! the embedded metadata.
//!
//! ## Structure
//!
//! A DNG's defining shape is an IFD **tree**, not a flat chain: IFD0 holds a small
//! preview/thumbnail and points, through the `SubIFDs` tag, at the full-resolution raw image in a
//! **sub-IFD**; EXIF lives in another sub-IFD. The encoder lays this tree out over `gamut-ifd`'s
//! tree-aware writer and composes the strip/tile pixel data around it; the decoder walks the tree
//! back to the raw samples and the parsed tags.
//!
//! ## Scope
//!
//! Reference: the **DNG 1.7.1.0 specification** (`references/dng/DNG_Spec_1_7_1_0.pdf`),
//! validated against the **Adobe DNG SDK 1.7.1** as the authoritative oracle (including its
//! real-libjxl JPEG XL reader and its `NewRawImageDigest` computation). Both directions are
//! full-surface:
//!
//! - **Layouts & compression**: strips and DNG-1.7 tiles; uncompressed, Deflate (encoded with
//!   [`gamut_deflate`], inflated by its shared bounded inflate under a cap derived from the chunk
//!   geometry), lossless JPEG (SOF3, coded by [`gamut_jpeg::lossless`]), and **JPEG XL** (Compression
//!   52546 — the Apple ProRAW codec; decode is pure-Rust jxl-rs, encode is the opt-in
//!   `jxl-encode` feature), with row/column interleave de-interleaving on decode.
//! - **Raw model**: CFA and `LinearRaw` photometries at 1–16 bits, the typed [`RawLevels`] level
//!   family, and the spec's chapter-5 raw-to-linear mapping as the explicit opt-in
//!   [`RawImage::to_linear`] (differentially gated against the SDK's stage-2 image).
//! - **Colour beyond the calibration**: the camera-profile tags [`CameraProfile`] does not model
//!   — the hue/saturation/value and look tables, the tone curve, the profile exposure offset, the
//!   DNG 1.6 third calibration set — decode as a typed [`ColorProfileInfo`], and the raw IFD's
//!   noise model as a typed [`NoiseProfile`].
//! - **Beyond the raw**: every other image IFD decodes as a typed [`SubImage`] — previews,
//!   transparency and **semantic masks** ([`SemanticMaskInfo`]), depth maps — and the
//!   gain-table maps ([`ProfileGainTableMap`], both tag versions) parse typed and re-serialise
//!   byte-exactly. Opcode lists are typed [`OpcodeList`] containers.
//! - **Integrity & explicitness**: the encoder writes the SDK-bit-exact `NewRawImageDigest`
//!   ([`RawImage::new_raw_image_digest`]), and the decoder surfaces unmodelled fields verbatim
//!   as typed [`RawTag`]s — from IFD 0, the raw IFD, every image sub-IFD, the last directory of
//!   the main chain when it is none of those, per page ([`PageExtra`]) every earlier main-chain
//!   page that is none of those either, and per directory ([`SubIfdExtra`]) every image-less
//!   directory reached through `SubIFDs`. One documented residue aside — a duplicated tag keeps
//!   the last entry (the IFD core's model) — nothing in a file is silently dropped, and
//!   [`deconstruct()`] accounts for every byte either way.
//!
//! An **Apple ProRAW** DNG (1.7, JPEG XL, tiled, LinearRaw, semantic masks, gain map) therefore
//! decodes fully. Full demosaicing and colour rendering are a raw *processor's* job and stay out
//! of scope, as does *executing* opcodes or gain maps. See `STATUS.md` for the per-feature
//! status and the deferred ledger (lossy JPEG, float samples, the opcode processing library).
//!
//! DNG codestreams are permanently backend-less: this crate exposes **no** pluggable codestream
//! backend (the IoC seam of #241), because the DNG compression schemes have no hardware
//! acceleration — gamut's software implementation is always used. (A format crate that does
//! expose its codestream does so through the one shared `gamut-codec-abi` seam: a typed backend
//! trait, a codec-abi adapter, and a push-order registry with the crate's own implementation as
//! the tail.) See `STATUS.md`.
//!
//! For archival use, [`deconstruct()`] classifies **every byte** of a DNG into typed segments
//! (dual-ledger checked, issue #263), and [`DngRewrite`] is the preserving edit path: the whole
//! tree opens losslessly, pixel payloads are carried byte-for-byte, and the `MakerNote` byte
//! range is pinned at its original offset.
//!
//! Memory-safe on hostile input: `#![forbid(unsafe_code)]` — like TIFF, DNG's offset-driven
//! structure is a classic parser-exploit surface, so the decoder is built to resist malformed
//! IFDs, offset loops, and truncation.
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

// One canonical path per item: every module is private and its public items are re-exported
// flat below. `tags` is the one namespace kept public — a deliberately low-level table of tag
// codes, not re-exported item by item.
pub mod tags;

mod bitpack;
mod color_profile;
mod compression;
mod decoder;
mod deconstruct;
mod digest;
mod encoder;
mod gain_map;
mod jxl;
mod levels;
mod linearize;
mod metadata;
mod opcode;
mod predictor;
mod preview;
mod profile;
mod raw;
mod rewrite;
mod subimage;
mod values;
mod whitebalance;
mod writer;

pub use color_profile::{ColorProfileInfo, HsvDelta, HsvTable, NoiseModel, NoiseProfile};
pub use compression::{is_decodable, is_lossy};
pub use decoder::{DecodedDng, DigestCheck, DngDecoder, PageExtra, RawTag, SubIfdExtra};
pub use deconstruct::deconstruct;
pub use encoder::{DngEncodeReport, DngEncoder};
pub use gain_map::{GainValues, ProfileGainTableMap};
// The C2PA surface (C2PA 2.4 §A.3.6, §18.5.5) is `gamut-ifd`'s — the placement and exclusion
// rules are stated once there for every TIFF-based codec — re-exported whole so a signer
// reading `DngEncodeReport`/`DecodedDng`, or checking the tag and the minimum store length this
// crate's own docs name (the latter `gamut-core`'s), needs no direct `gamut-ifd` dependency (the
// re-export closure of `STATUS.md`'s freeze decisions).
// The non-validating report a host prints — `c2pa_summary`, the store `DecodedDng::c2pa_exclusions`
// names plus every tag-52545 entry that yields none, with the reason — is gamut-ifd's too, so it
// is stated once for TIFF and DNG alike (issue #530).
pub use gamut_core::Dimensions;
pub use gamut_core::c2pa::MIN_STORE_LEN;
// `DngMetadata::exif` is gamut-exif's model, as `gamut-tiff`'s `TiffMetadata::exif` is;
// re-exported with the tag vocabulary its accessors take so a caller needs no direct gamut-exif
// dependency.
pub use gamut_exif::{Exif, ExifTag};
// The structural IFD core is gamut-ifd's; the same subset gamut-tiff re-exports, so no direct
// gamut-ifd dependency is needed to name any type this crate's own items reach. `Value` (with
// `UnknownValue`) is the decode surface `RawTag` carries unmodelled fields as; `Ifd` (with
// `Field`/`FieldType`/`SubIfd`) is the directory type of the `Exif` model's sub-IFDs and what
// `DngRewrite` edits inside a `TiffFile`; `Variant` selects BigTIFF on the encoder; the deconstruct report
// (`DeconstructReport` and its `Anomaly`/`Severity`/`UnknownTag`/`UnknownFieldType` findings,
// `SegmentReport` with its `Segment`/`SpanKind`/`DataLabel`/`Conflict`/`SharedSpan`) is shared
// with gamut-tiff, its ranges plain `core::ops::Range<u64>`.
pub use gamut_ifd::{
    Anomaly, ByteOrder, Conflict, DataLabel, DeconstructReport, Field, FieldType, Ifd, Segment,
    SegmentReport, Severity, SharedSpan, SpanKind, SubIfd, TiffFile, UnknownFieldType, UnknownTag,
    UnknownValue, Value, Variant,
};
pub use gamut_ifd::{
    C2PA_MANIFEST_STORE, C2PA_NOT_VALIDATED, C2paExclusions, C2paStoreSummary, C2paSummary,
    C2paUnreadEntry, C2paUnreadReason, c2pa_summary,
};
// The JPEG XL encoder knobs are gamut-jxl's own validated types, re-exported because
// `DngEncoder::with_jxl_distance` / `with_jxl_effort` take them.
pub use gamut_jxl::{Distance, Effort};
// The facade types named in the `metadata`-feature signatures — `DngMetadata`'s `TypedMetadata`
// impl and `DngEncoder`'s `MetadataSink` impl — so a caller can spell them without a direct
// gamut-metadata dependency.
#[cfg(feature = "metadata")]
pub use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, MetadataSink, TypedMetadata};
pub use levels::{MAX_BLACK_REPEAT, RawLevels};
pub use linearize::LinearImage;
pub use metadata::DngMetadata;
pub use opcode::{Opcode, OpcodeList, opcode_id};
pub use profile::CameraProfile;
pub use raw::{RawImage, RawPhotometry, cfa_color};
pub use rewrite::{DngRewrite, MakerNotePreservation, PreservedSpan, RewrittenDng};
pub use subimage::{
    DepthInfo, MaskSubArea, SemanticMaskInfo, SubImage, SubImageData, SubImageKind,
};
pub use values::{
    CalibrationIlluminant, CfaLayout, Compression, PhotometricInterpretation, Predictor,
    PreviewColorSpace, ProfileEmbedPolicy, SampleFormat, TableEncoding, new_subfile_type,
};
