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
//!   [`gamut_deflate`], inflated with `miniz_oxide` under a cap derived from the chunk geometry),
//!   lossless JPEG (the public SOF3 [`lossless_jpeg`] module), and **JPEG XL** (Compression
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
//!   ([`RawImage::new_raw_image_digest`]), and the decoder surfaces every unmodelled field
//!   verbatim as typed [`RawTag`]s — nothing in a file is silently dropped.
//!
//! An **Apple ProRAW** DNG (1.7, JPEG XL, tiled, LinearRaw, semantic masks, gain map) therefore
//! decodes fully. Full demosaicing and colour rendering are a raw *processor's* job and stay out
//! of scope, as does *executing* opcodes or gain maps. See `STATUS.md` for the per-feature
//! status and the deferred ledger (lossy JPEG, float samples, the opcode processing library).
//!
//! DNG codestreams are permanently backend-less: this crate exposes **no** pluggable codestream
//! backend (the IoC seam of #241), because the DNG compression schemes have no hardware
//! acceleration — gamut's software implementation is always used. See AGENTS.md's convention on
//! exposing the codestream and `STATUS.md`.
//!
//! For archival use, [`deconstruct`] classifies **every byte** of a DNG into typed segments
//! (dual-ledger checked, issue #263), and [`DngRewrite`] is the preserving edit path: the whole
//! tree opens losslessly, pixel payloads are carried byte-for-byte, and the `MakerNote` byte
//! range is pinned at its original offset.
//!
//! Memory-safe on hostile input: `#![forbid(unsafe_code)]` — like TIFF, DNG's offset-driven
//! structure is a classic parser-exploit surface, so the decoder is built to resist malformed
//! IFDs, offset loops, and truncation.
#![forbid(unsafe_code)]

pub mod color_profile;
pub mod decoder;
pub mod deconstruct;
pub mod encoder;
pub mod gain_map;
pub mod levels;
pub mod linearize;
pub mod lossless_jpeg;
pub mod metadata;
pub mod opcode;
pub mod profile;
pub mod raw;
pub mod rewrite;
pub mod subimage;
pub mod tags;
pub mod values;

mod bitpack;
mod compression;
mod digest;
mod jxl;
mod md5;
mod predictor;
mod preview;
mod whitebalance;
mod writer;

// The shared error/result/dimension types every gamut codec speaks, re-exported so callers need
// not also depend on `gamut-core` directly, along with the byte-order selector from the IFD core.
pub use color_profile::{ColorProfileInfo, HsvDelta, HsvTable, NoiseModel, NoiseProfile};
pub use decoder::{DecodedDng, DigestCheck, DngDecoder, RawTag};
pub use deconstruct::{
    Anomaly, DeconstructReport, Severity, UnknownFieldType, UnknownTag, deconstruct,
};
pub use encoder::{DngEncodeReport, DngEncoder};
pub use gain_map::{GainValues, ProfileGainTableMap};
pub use gamut_core::{Dimensions, Error, Result};
// The C2PA surface (C2PA 2.4 §A.3.6, §18.5.5) is `gamut-ifd`'s — the placement and exclusion
// rules are stated once there for every TIFF-based codec — re-exported whole so a signer
// reading `DngEncodeReport`/`DecodedDng`, or checking the tag and the minimum store length this
// crate's own docs name, needs no direct `gamut-ifd` dependency (the re-export closure of
// `STATUS.md`'s freeze decisions).
pub use gamut_ifd::c2pa::{C2PA_MANIFEST_STORE, C2paExclusions, MIN_STORE_LEN};
// `Value` is part of the decode surface: `RawTag` carries unmodelled fields as this typed enum;
// `Segment`/`SpanKind`/`Range` are part of the preservation surface, naming the byte runs a
// real camera file carries that its own structures do not account for (`Range` is also what
// `C2paExclusions` measures in).
pub use gamut_ifd::{ByteOrder, Range, Segment, SpanKind, Value};
// The shared metadata facade supplies this crate's metadata models rather than a DNG-local
// restatement of them: `DngMetadata::exif` *is* the facade's `Exif`, and `DngMetadata::blocks`
// hands the byte carriers over as `MetadataBlock`s. Re-exported so a caller can build and read
// that surface without also depending on `gamut-metadata` directly.
pub use gamut_metadata::MetadataBlock;
pub use gamut_metadata::exif::{Exif, ExifTag, Rational};
pub use levels::RawLevels;
pub use linearize::LinearImage;
pub use lossless_jpeg::LosslessJpeg;
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
