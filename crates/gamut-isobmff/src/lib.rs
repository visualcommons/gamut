//! ISO Base Media File Format (ISOBMFF) container for still images — the structural layer the AVIF
//! and HEIC codecs share.
//!
//! This crate owns the *container*, not the codec: it models the box tree of a single-image ISOBMFF
//! file (`ftyp` + a `meta` box of image items + `mdat`) and leaves the coded bitstream opaque
//! ([`PropertyKind::CodecConfiguration`] for the `av1C`/`hvcC` record, [`Item::payload`] for the
//! samples). [`write()`] serialises an [`IsoBmffImage`]; [`read`] parses one back. The two are
//! inverse for any file this crate writes (`read(&write(&img)?) == img`) — which is why [`write()`]
//! validates the model first and refuses what could not come back, a `top_level_boxes` list that
//! interleaves positions among it.
//!
//! It is image-first: the modelled surface is the HEIF still-image set — `ftyp`, `meta` with
//! `hdlr`/`pitm`/`iloc`/`iinf`/`iref`/`iprp`/`idat`/`grpl`, the
//! `ispe`/`pixi`/`colr`/`irot`/`imir`/`clap`/`pasp`/`auxC`/`clli` properties, typed item
//! references (`auxl`/`cdsc`/`dimg`/`thmb`/`prem`, …) and entity groups — plus opaque
//! codec-configuration and unrecognised properties carried verbatim, and any top-level box the
//! model does not otherwise own ([`IsoBmffImage::top_level_boxes`]: a C2PA `uuid`
//! `ContentProvenanceBox`, a `free`, a vendor box), written after `ftyp` or after `mdat` per its
//! [`TopLevelPosition`]. The writer normalises to the
//! smallest box versions (`iloc` v0, single extent into `mdat`); the reader additionally accepts
//! the foreign-encoder repertoire (`iloc` v1/v2, `idat` placement, multi-extent payloads, 32-bit
//! item ids, 16-bit `ipma` indices). Image sequences/tracks and item protection are out of scope —
//! see this crate's `STATUS.md` for the deferred/out-of-scope ledger. Box byte layouts follow
//! ISO/IEC 14496-12 (ISOBMFF) and ISO/IEC 23008-12 (HEIF); see `references/isobmff`.
//!
//! [`read`] models only the *primary* still-image stream. It is tolerant of real-world "motion
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
//! use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind, TopLevelBox, read, write};
//!
//! // The C2PA 2.4 §A.5.1 `ContentProvenanceBox` user type; the payload is opaque here.
//! const C2PA_UUID: [u8; 16] = [
//!     0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
//! ];
//! let img = IsoBmffImage::new(
//!     *b"avif",
//!     vec![*b"avif", *b"mif1", *b"miaf"],
//!     1,
//!     vec![Item {
//!         id: 1,
//!         item_type: *b"av01",
//!         name: String::new(),
//!         content_type: None,
//!         content_encoding: None,
//!         hidden: false,
//!         references: vec![],
//!         properties: vec![Property {
//!             essential: false,
//!             kind: PropertyKind::ImageSpatialExtents { width: 64, height: 64 },
//!         }],
//!         payload: vec![1, 2, 3, 4], // the coded bitstream (opaque to this crate)
//!     }],
//! )
//! .with_top_level_boxes(vec![TopLevelBox::uuid(C2PA_UUID, b"manifest-store".to_vec())]);
//! let bytes = write(&img).unwrap();
//! assert_eq!(read(&bytes).unwrap(), img);
//! ```
#![forbid(unsafe_code)]

mod boxes;
mod grid;
mod model;
mod overlay;
mod reader;
mod segments;
mod writer;

pub use boxes::{BoxReader, RawBox};
pub use grid::ImageGrid;
pub use model::{
    ColourInformation, EntityGroup, IsoBmffImage, Item, ItemReference, NclxColr, Property,
    PropertyKind, TopLevelBox, TopLevelPosition,
};
pub use overlay::ImageOverlay;
pub use reader::read;
pub use segments::{
    Segment, SegmentKind, UnknownBox, UnknownBoxLocation, walk_meta_children, walk_segments,
};
pub use writer::write;
