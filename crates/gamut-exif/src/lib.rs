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
//! [`parse_with_report`](ExifReader::parse_with_report) returns a [`ReadReport`] naming every
//! sub-IFD and thumbnail range the lenient reader discarded. `parse` is the `&[u8]` case of
//! `parse_from` and stays silent, so neither is a change for existing callers.
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

pub mod error;
pub mod exif;
pub mod gps;
pub mod maker_note;
pub mod reader;
pub mod report;
pub mod stream;
pub mod tag;
pub mod thumbnail;
pub mod value;
pub mod writer;

// EXIF is a TIFF profile: reuse the container's value and directory types directly rather than
// wrapping them in a parallel model.
pub use error::{ExifError, Result};
pub use exif::Exif;
pub use gamut_ifd::{ByteOrder, Ifd, Value};
#[cfg(feature = "geocoordinates")]
pub use gps::GpsConversionError;
pub use gps::{GpsAltitude, GpsCoordinate, GpsInfo, GpsReference};
pub use maker_note::{MakerNote, MakerNoteVendor};
pub use reader::ExifReader;
pub use report::{DropReason, Dropped, DroppedRegion, ReadReport};
pub use tag::{ExifTag, IfdKind};
pub use thumbnail::Thumbnail;
pub use value::{Rational, SRational, as_text};
pub use writer::ExifWriter;
