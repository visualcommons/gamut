//! ISO Base Media File Format (ISOBMFF) container for still images — the structural layer the AVIF
//! and HEIC codecs share.
//!
//! This crate owns the *container*, not the codec: it models the box tree of a single-image ISOBMFF
//! file (`ftyp` + a `meta` box of image items + `mdat`) and leaves the coded bitstream opaque
//! ([`PropertyKind::CodecConfiguration`] for the `av1C`/`hvcC` record, [`Item::payload`] for the
//! samples). [`IsoBmffImage::to_bytes`] serialises an [`IsoBmffImage`]; [`IsoBmffImage::parse`]
//! parses one back. The two are inverse for any file this crate writes
//! (`IsoBmffImage::parse(&img.to_bytes()?)? == img`) — which is why `to_bytes` validates the model
//! first and refuses what could not come back, a `top_level_boxes` list that interleaves positions
//! among it.
//!
//! It is image-first: the modelled surface is the HEIF still-image set — `ftyp`, `meta` with
//! `hdlr`/`pitm`/`iloc`/`iinf`/`iref`/`iprp`/`idat`/`grpl`, the
//! `ispe`/`pixi`/`colr`/`irot`/`imir`/`clap`/`pasp`/`auxC`/`clli`/`mdcv` properties, typed item
//! references (`auxl`/`cdsc`/`dimg`/`thmb`/`prem`, …) and entity groups — plus opaque
//! codec-configuration and unrecognised properties carried verbatim, and any top-level box the
//! model does not otherwise own ([`IsoBmffImage::top_level_boxes`]: a C2PA `uuid`
//! `ContentProvenanceBox`, a `free`, a vendor box), written after `ftyp` or after `mdat` per its
//! [`TopLevelPosition`]. The C2PA framing of that `uuid` box — its user type, `box_purpose`,
//! merkle offset and store slot — is read by [`C2paUuidBox::classify`] and written by
//! [`content_provenance_payload`], once for both codecs; so is the `Exif` item's
//! `exif_tiff_header_offset` framing (ISO/IEC 23008-12 §A.2.1), by [`Item::exif_tiff_stream`].
//! The writer normalises to the
//! smallest box versions (`iloc` v0, single extent into `mdat`); the reader additionally accepts
//! the foreign-encoder repertoire (`iloc` v1/v2, `idat` placement, multi-extent payloads, 32-bit
//! item ids, 16-bit `ipma` indices). Image sequences/tracks and item protection are out of scope —
//! [`IsoBmffImage::parse`] skips a sequence's `moov` so the `meta` primary still beside it reads,
//! and refuses a sequence-only file — see this crate's `STATUS.md` for the deferred/out-of-scope
//! ledger. Box byte layouts follow ISO/IEC 14496-12 (ISOBMFF) and ISO/IEC 23008-12 (HEIF); see
//! `references/isobmff`.
//!
//! [`IsoBmffImage::parse`] models only the *primary* still-image stream. It is tolerant of real-world "motion
//! photo" files that append a second, foreign stream (a Samsung MP4 starting with a second `ftyp`
//! and its own `moov`, a Google `mpvd` box, or a trailing non-ISOBMFF vendor blob): the top-level
//! walk stops cleanly at a second `ftyp` and at any malformed trailing box once `ftyp`+`meta` have
//! been seen, so the semantic model covers the primary image and nothing else. Mapping every byte
//! of the file — the remainder included — to a box, an appended stream or a trailer is
//! [`walk_segments`], which lives here rather than in a consumer since #436: `gamut-avif` and
//! `gamut-heic` had a copy each, and after normalising the format names the two were identical.
//! The box-walk primitives [`BoxReader`] and [`RawBox`] stay re-exported for a consumer that
//! wants to account for something this walk does not model.
//!
//! ```
//! use gamut_isobmff::{
//!     C2PA_UUID, C2paBoxPurpose, IsoBmffImage, Item, Property, PropertyKind, TopLevelBox,
//!     content_provenance_payload,
//! };
//!
//! // A C2PA 2.4 §A.5.1 `ContentProvenanceBox`; the store in it is opaque here.
//! let provenance = content_provenance_payload(C2paBoxPurpose::Manifest, b"manifest-store");
//! let img = IsoBmffImage::new(
//!     *b"avif",
//!     vec![*b"avif", *b"mif1", *b"miaf"],
//!     1,
//!     vec![
//!         Item::new(1, *b"av01")
//!             .with_properties(vec![Property::new(
//!                 false,
//!                 PropertyKind::ImageSpatialExtents { width: 64, height: 64 },
//!             )])
//!             .with_payload(vec![1, 2, 3, 4]), // the coded bitstream (opaque to this crate)
//!     ],
//! )
//! .with_top_level_boxes(vec![TopLevelBox::uuid(C2PA_UUID, provenance)]);
//! let bytes = img.to_bytes().unwrap();
//! assert_eq!(IsoBmffImage::parse(&bytes).unwrap(), img);
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod boxes;
mod c2pa;
mod grid;
mod model;
mod overlay;
mod reader;
mod segments;
mod writer;

pub use boxes::{BoxReader, RawBox};
pub use c2pa::{
    C2PA_UUID, C2paBoxPurpose, C2paSlot, C2paUnreadReason, C2paUuidBox, content_provenance_payload,
    content_provenance_reserved,
};
pub use grid::ImageGrid;
pub use model::{
    CleanAperture, ColourInformation, ContentLightLevel, EntityGroup, IsoBmffImage, Item,
    ItemReference, MasteringDisplay, PixelAspectRatio, Property, PropertyKind, TopLevelBox,
    TopLevelPosition,
};
pub use overlay::{ImageOverlay, OverlayOffset};
pub use segments::{
    Segment, SegmentKind, SegmentWalk, UnknownBox, UnknownBoxLocation, walk_meta_children,
    walk_segments,
};
