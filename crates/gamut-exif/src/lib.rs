//! `gamut-exif` — EXIF image metadata parsing and serialization.
//!
//! An EXIF blob is an optional `Exif\0\0` marker followed by a TIFF stream (the JPEG `APP1`
//! payload, the WebP `EXIF` chunk, the PNG `eXIf` chunk, the AVIF/HEIF `Exif` item). Its structure
//! is a chain of IFDs — the 0th (primary image) and 1st (thumbnail) directories, plus the Exif,
//! GPS, and Interoperability sub-IFDs reached through pointer tags — so this crate builds on the
//! shared [`gamut_ifd`](https://crates.io/crates/gamut-ifd) TIFF/IFD core and adds the EXIF tag
//! dictionary, typed value access, GPS/thumbnail models, and the sub-IFD layout on top.
//!
//! Because EXIF *is* a constrained profile of TIFF, the value model is [`gamut_ifd::Value`] itself
//! — re-exported here rather than duplicated — and the directories are [`gamut_ifd::Ifd`]s reached
//! through [`Exif`]'s accessors. Tags and semantics follow **Exif 3.0** (CIPA DC-008;
//! `references/exif`), with Exif 2.32 retained for legacy tag compatibility.
//!
//! [`Exif::parse`] reads a blob and [`Exif::to_bytes`] re-serialises it (preserving the byte order);
//! read tags with the typed accessors or the [`ExifTag`] catalogue.
//!
//! Two further read entry points sit on [`ExifReader`]:
//! [`parse_from`](ExifReader::parse_from) reads through [`gamut_ifd::ReadAt`] rather than a slice,
//! so EXIF can be pulled out of a large raw file without loading it, and
//! [`parse_with_report`](ExifReader::parse_with_report) returns a [`ReadReport`] naming the
//! sub-IFDs, thumbnail ranges and trailing directories the lenient reader discarded — see
//! [`report`] for what that covers and what it deliberately does not. `parse` is the `&[u8]` case
//! of `parse_from` and stays silent, so neither is a change for existing callers.
//!
//! Writing has one extra door. Every tag CIPA DC-008 itself defines carries the field type and
//! component count that specification mandates for it ([`ExifTag::field_types`],
//! [`ExifTag::component_count`]), and [`set_tag_checked`] refuses a value that contradicts them.
//! A handful of catalogued tags come from other specifications and are carried only for
//! compatibility; DC-008 mandates nothing for them, their `field_types` is empty, and
//! [`set_tag_checked`] claims no constraint — see [`tag`] for which they are.
//!
//! Everything else on the write path is lenient on purpose, because a caller reproducing a
//! non-conformant source file must be able to: [`Exif::set_tag`], [`Exif::set`],
//! [`Exif::exif_ifd_mut`], [`Exif::gps_ifd_mut`], [`Exif::interop_ifd_mut`] and
//! [`Exif::set_gps_ifd`] all write whatever they are given, as do their `image_mut` /
//! `set_exif_ifd` / `set_interop_ifd` siblings. So does the entire read path.
//!
//! The optional `describe` feature (default **off**) adds the `describe` module: what the
//! enumerated tags' values *mean*, in the specification's own wording, plus `describe::flash` for
//! the `Flash` bitfield. It is a few kilobytes of static strings a consumer that only writes or
//! round-trips metadata never reads, which is why it is opt-in. (Those names are not linked here:
//! the module does not exist when the feature is off.) Only a crate that depends on `gamut-exif`
//! directly can turn it on: neither the `gamut-metadata` facade nor the `gamut` umbrella forwards
//! it yet.
//!
//! ```
//! use gamut_exif::{ByteOrder, Exif, ExifTag, Value};
//!
//! let mut exif = Exif::new(ByteOrder::LittleEndian);
//! exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
//! exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
//!
//! let bytes = exif.to_bytes()?; // Exif\0\0 + TIFF, ready to embed
//! let parsed = Exif::parse(&bytes)?;
//! assert_eq!(parsed.make(), Some("gamut"));
//! assert_eq!(parsed.f_number().and_then(|r| r.to_f64()), Some(2.8));
//! # Ok::<(), gamut_exif::ExifError>(())
//! ```
#![forbid(unsafe_code)]

#[cfg(feature = "describe")]
pub mod describe;
pub mod error;
pub mod exif;
pub mod gps;
pub mod maker_note;
pub mod reader;
pub mod report;
mod stream;
pub mod tag;
pub mod thumbnail;
pub mod value;
pub mod writer;

// EXIF is a TIFF profile: reuse the container's value and directory types directly rather than
// wrapping them in a parallel model.
#[cfg(feature = "describe")]
pub use describe::{FlashDescription, describe, described_values, flash};
pub use error::{ExifError, Result};
pub use exif::Exif;
pub use gamut_ifd::{ByteOrder, FieldType, Ifd, Value};
#[cfg(feature = "geocoordinates")]
pub use gps::GpsConversionError;
pub use gps::{GpsAltitude, GpsCoordinate, GpsInfo, GpsReference};
pub use maker_note::{MakerNote, MakerNoteVendor};
pub use reader::ExifReader;
pub use report::{DropReason, Dropped, DroppedRegion, ReadReport};
pub use tag::{ExifTag, IfdKind, TagCount};
pub use thumbnail::Thumbnail;
pub use value::{Rational, SRational, as_text};
pub use writer::{ExifWriter, TagConstraintError, check_tag, set_tag_checked};
