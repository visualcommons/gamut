//! HEIC still-image decoding: the HEVC binding over `gamut-heif`'s shared HEIF layer.
//!
//! HEIC is HEIF (ISO/IEC 23008-12) around HEVC intra pictures. Everything outside the coded
//! picture — the item model, the `grid`/`iden`/`iovl` derivations, the decode limits, YCbCr → RGB,
//! alpha, the `clap`/`irot`/`imir` transforms and the C2PA summary — is `gamut-heif`'s, written
//! once for AVIF and HEIC. What is this crate's is the HEVC side of it: the `hvcC` record, the NAL
//! layer, the still-image constraint, the [`Hevc`] binding that plugs them into `gamut-heif`, the
//! [`HevcCodestreamDecoder`] backends that decode the coded picture, and the byte-accounting
//! [`HeifContainer`]. gamut is **decode-only** for this format (see `references/heif`), and ships
//! no HEVC codestream decoder: the coded picture is decoded by a backend the caller adds (HEVC
//! pixel reconstruction is codec scope, issue #18).
//!
//! # Decoding
//!
//! [`HeifDecoder`] decodes a file's primary image through [`DecodeImage`](gamut_core::DecodeImage)
//! — to [`Rgba8`](gamut_core::Rgba8) (8-bit coded frames) or [`Rgba16`](gamut_core::Rgba16) (any
//! coded depth from 8 to 16 bits, 10-bit BT.2020 HDR included, normalised to the full 16-bit range)
//! — or to its planar samples through [`HeifDecoder::decode_planar`], under [`DecodeLimits`]. Its
//! backends are [`HevcCodestreamDecoder`]s added with [`HeifDecoder::with_backend`] and kept in
//! a [`gamut_codec_abi::Registry`]: tried in the order they were added, `supports` returning
//! `false` the only fall-through, an accepting backend's error returned unchanged.
//! [`AbiHevcDecoder`] adapts a [`gamut_codec_abi::Decoder`] — a pure-Rust one, or a C backend
//! through [`gamut_codec_abi::bridge::ForeignDecoder`] — into that trait.
//!
//! To decode an item other than the primary one, name the [`Hevc`] binding on `gamut-heif`'s
//! surfaces: `container.image().decode_item_rgba8::<Hevc>(id, &backends, options)`.
//!
//! # The container
//!
//! [`HeifContainer`] is the **total, byte-accounting** representation. Its
//! [`segments`](HeifContainer::segments) are contiguous, non-overlapping, and cover every byte of
//! the input, so it is *structurally impossible to ignore any bits*: unknown top-level boxes are
//! surfaced verbatim ([`SegmentKind::Box`]), an appended vendor stream (a second top-level `ftyp`,
//! e.g. a Samsung motion-photo MP4) is retained opaquely ([`SegmentKind::AppendedStream`]), and
//! trailing non-box bytes become an explicit [`SegmentKind::Trailer`]. Boxes inside `meta` that
//! the semantic parse does not consume are surfaced as [`UnknownBox`]es. Its
//! [`image`](HeifContainer::image) is `gamut-heif`'s role-typed [`HeifImage`];
//! [`is_hevc_still`](HeifContainer::is_hevc_still) and
//! [`hevc_config`](HeifContainer::hevc_config) are the HEVC questions about it.
//!
//! [`HeifContainer::c2pa`] locates the C2PA manifest store in a top-level `uuid`
//! `ContentProvenanceBox` and reports it as opaque bytes plus its exact byte range
//! ([`C2paManifestStore`] — a locator, never a validator); [`HeifContainer::c2pa_summary`] is the
//! same scan shaped for *reporting* ([`C2paSummary`], with the [`C2PA_NOT_VALIDATED`] disclaimer).
//!
//! # HEVC configuration & NAL layer
//!
//! [`HevcConfig::parse`] decodes an `hvcC` record (`references/heif` §1) into typed
//! profile/tier/level, chroma/bit-depth, and the parameter-set [`arrays`](HevcConfig::arrays).
//! [`iter_nal_units`] splits a length-prefixed `hvc1`/`hev1` payload (§2), [`NalHeader::parse`]
//! reads the two-byte NAL header and [`NalUnitType`] classifies it (§3),
//! [`HevcConfig::validate_still_payload`] enforces the still-image IRAP constraint, and
//! [`HevcConfig::annex_b`] converts a payload to a start-coded NAL stream for a downstream decoder.
//!
//! # Feeding a platform decoder
//!
//! Hardware HEVC decoders disagree on how the parameter sets and the coded picture should arrive,
//! so the three shapes are addressable separately. The item payload is *always* the length-prefixed
//! stream stored in the file ([`HevcConfig::nal_length_size`] gives the prefix width); the Annex-B
//! emitters append to a caller-owned buffer, so a scratch `Vec<u8>` can be reused across items.
//!
//! | Target | Parameter sets | Coded picture |
//! | --- | --- | --- |
//! | Apple VideoToolbox (`CMVideoFormatDescription`) | raw `hvcC` bytes from [`HeifItem::codec_configuration`], or the bare NAL units from [`HevcConfig::vps`]/[`sps`](HevcConfig::sps)/[`pps`](HevcConfig::pps) with `nalUnitHeaderLength` = [`nal_length_size`](HevcConfig::nal_length_size) | the item payload verbatim — **no** Annex-B conversion |
//! | VAAPI / FFmpeg / libde265 (raw Annex-B) | [`HevcConfig::annex_b`] emits both in one buffer | — |
//! | Android MediaCodec | [`HevcConfig::annex_b_parameter_sets`] ⇒ the `csd-0` buffer | [`HevcConfig::annex_b_payload`] ⇒ the sample buffers |
//!
//! Parameter sets may also arrive inband: an `hev1` item is allowed to carry them in the payload
//! (and an `hvc1` item never does —
//! [`HeifContainer::hevc_inband_parameter_sets_allowed`]). The emitters do not de-duplicate, so an
//! `hev1` stream can repeat them; H.265 decoders accept that.
//!
//! # Conformance
//!
//! The whole pipeline is checked differentially against **libheif + libde265** — plugged in as an
//! [`HevcCodestreamDecoder`] — over gamut-authored fixtures generated at test time
//! (`tests/conformance.rs`, the dev-only `tooling/libheif-oracle`; see `references/heif` "Oracle").
//!
//! # Metadata
//!
//! [`HeifDecoder::metadata`] hands over the Exif item's TIFF stream (its
//! `exif_tiff_header_offset` applied), the XMP item's packet and the primary item's `colr` ICC
//! profile as a [`RawMetadata`]. With the optional **`metadata`** Cargo
//! feature (off by default) the facade's `TypedMetadata` is re-exported, which parses that into
//! a unified `Metadata`. A C2PA manifest store lives outside the item model, in a top-level
//! `uuid` box; [`HeifContainer::c2pa`] locates it.
//!
//! # Out of scope
//!
//! Image *sequences* (`msf1`/`hevc`/`hevx` tracks) are permanently out of scope (gamut is
//! image-first). See this crate's `STATUS.md`.
//!
//! # Example
//!
//! Decode the primary image through a caller-supplied backend. The stub here returns a solid-grey
//! monochrome frame; a real one wraps a platform HEVC decoder (libde265, VideoToolbox, …).
//!
//! ```
//! use gamut_color::ChromaSubsampling;
//! use gamut_core::{DecodeImage, Dimensions, ImageBuf, Result, Rgba8};
//! use gamut_heic::{DecodedFrame, HeifContainer, HeifDecoder, HevcCodestreamDecoder, HevcConfig};
//! use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind};
//!
//! // A stub HEVC decoder: ignores the codestream and returns a 2x2 solid-grey monochrome frame.
//! struct GreyStub;
//! impl HevcCodestreamDecoder for GreyStub {
//!     fn supports(&mut self, _config: &HevcConfig) -> bool {
//!         true
//!     }
//!     fn decode_still(&mut self, _config: &HevcConfig, _payload: &[u8]) -> Result<DecodedFrame> {
//!         DecodedFrame::new(Dimensions::new(2, 2)?, 8, ChromaSubsampling::Cs400, vec![128; 4], vec![], vec![])
//!     }
//! }
//!
//! # fn hvcc() -> Vec<u8> {
//! #     // Minimal valid hvcC: 23-byte header, lengthSizeMinusOne = 3, numOfArrays = 0.
//! #     let mut v = vec![0u8; 23];
//! #     v[0] = 1; // configurationVersion
//! #     v[21] = 0b0000_0011; // ... | lengthSizeMinusOne = 3
//! #     v[22] = 0; // numOfArrays
//! #     v
//! # }
//! // A minimal HEVC still: one hvc1 item with an hvcC config and a single IRAP (IDR) NAL payload.
//! let img = IsoBmffImage::new(
//!     *b"heic",
//!     vec![*b"heic", *b"mif1"],
//!     1,
//!     vec![
//!         Item::new(1, *b"hvc1")
//!             .with_properties(vec![Property::new(
//!                 true,
//!                 PropertyKind::CodecConfiguration { kind: *b"hvcC", data: hvcc() },
//!             )])
//!             // 4-byte length prefix + an IDR_W_RADL NAL (type 19: header byte 0x26).
//!             .with_payload(vec![0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD]),
//!     ],
//! );
//! let bytes = img.to_bytes().unwrap();
//!
//! let container = HeifContainer::parse(&bytes).unwrap();
//! assert!(container.is_hevc_still());
//! assert_eq!(container.image().primary_item().id(), 1);
//!
//! // Decode the primary image to RGBA through the stub decoder.
//! let rgba: ImageBuf<Rgba8> = HeifDecoder::new().with_backend(GreyStub).decode_image(&bytes).unwrap();
//! assert_eq!((rgba.width(), rgba.height()), (2, 2));
//! assert_eq!(rgba.as_samples()[3], 255); // opaque (no alpha auxiliary)
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod backend;
mod c2pa;
mod container;
mod decoder;
mod hvcc;
mod nal;
mod sps;

pub use backend::{AbiHevcDecoder, HEVC_CODEC_ID, HevcCodestreamDecoder};
pub use container::HeifContainer;
pub use decoder::{HeifDecoder, Hevc};
// The foreign types this crate's signatures name, so a caller needs no direct dependency to spell
// them.
pub use gamut_color::{ChromaSubsampling, ChromaUpsampling};
pub use gamut_core::{Dimensions, Orientation, RawMetadata};
// The shared HEIF layer: the item model `HeifContainer::image` returns and the types its accessors
// return, the decode-surface types, and the C2PA locator and summary.
pub use gamut_heif::{
    C2PA_NOT_VALIDATED, C2paBoxPosition, C2paManifestStore, C2paMerkleBox, C2paPresence,
    C2paStoreSummary, C2paSummary, C2paUnreadBox, CleanAperture, ContentLightLevel, DecodeLimits,
    DecodeOptions, DecodedFrame, HeifImage, HeifItem, ItemKind, MasteringDisplay, PixelAspectRatio,
    TransformativeProperty,
};
// The C2PA `ContentProvenanceBox` framing and the segment walk are `gamut-isobmff`'s.
pub use gamut_isobmff::{
    C2PA_UUID, C2paBoxPurpose, C2paUnreadReason, Segment, SegmentKind, UnknownBox,
    UnknownBoxLocation,
};
// The facade types named in the `metadata`-feature docs, and `TypedMetadata`, which parses the
// `RawMetadata` `HeifDecoder::metadata` returns.
#[cfg(feature = "metadata")]
pub use gamut_metadata::{Metadata, MetadataBlock, TypedMetadata};
pub use hvcc::{HevcConfig, NalArray};
pub use nal::{NalHeader, NalUnitIter, NalUnitType, iter_nal_units};
