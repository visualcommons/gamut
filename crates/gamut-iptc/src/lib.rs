//! `gamut-iptc` — IPTC photo metadata parsing and serialization.
//!
//! IPTC photo metadata exists in two forms, and this crate covers both:
//!
//! - **Legacy IIM** (Information Interchange Model) — a binary record/dataset stream, in practice
//!   embedded inside a Photoshop Image Resource Block (resource id `0x0404`, an `8BIM` block) within
//!   a JPEG `APP13` segment. Implemented in [`iim`] (the dataset codec), [`irb`] (the `8BIM`
//!   carrier), and [`charset`] (the dataset 1:90 coded character set).
//! - **IPTC Photo Metadata** (Core + Extension) — the modern standard, serialized **as XMP**.
//!   Modelled in [`photo_metadata`] on top of [`gamut_xmp`]'s property graph.
//!
//! The two overlap heavily; reconciling them (which value wins when both carry the same datum) is
//! the crate's keystone, surfaced through [`IptcReader::read`] (merge, with a [`ConflictPolicy`])
//! and [`IptcWriter::write_iim`] (projection back to IIM).
//!
//! Standards: **IPTC-IIM 4.2** and the **IPTC Photo Metadata Standard** (`references/iptc`).
//!
//! # Reading and writing legacy IIM
//!
//! ```
//! use gamut_iptc::{IimBlock, IimCharset, IimDataSet, IptcReader, PhotoshopIrb};
//!
//! // Build some descriptive datasets and serialize them into an 8BIM resource stream.
//! let block = IimBlock {
//!     datasets: vec![
//!         IimDataSet { record: 2, dataset: 0, data: vec![0, 4] }, // Record Version = 4
//!         IimDataSet { record: 2, dataset: 25, data: b"sky".to_vec() }, // Keywords
//!     ],
//! };
//! let irb = PhotoshopIrb::with_iptc(block.encode()?).encode()?;
//!
//! // Read it back and decode a text value with the stream's charset.
//! let parsed = IptcReader::new().read_irb(&irb)?.expect("0x0404 resource present");
//! let charset = IimCharset::detect(&parsed)?;
//! assert_eq!(charset.decode(&parsed.datasets[1].data)?, "sky");
//! # Ok::<(), gamut_iptc::IptcError>(())
//! ```
//!
//! # Reconciling the two carriers
//!
//! [`IptcReader::read`] merges legacy IIM and modern XMP into one [`PhotoMetadata`] view with typed
//! accessors:
//!
//! ```
//! use gamut_iptc::{IimBlock, IimDataSet, IptcReader};
//!
//! let iim = IimBlock { datasets: vec![
//!     IimDataSet { record: 2, dataset: 90, data: b"Lyon".to_vec() },  // City
//!     IimDataSet { record: 2, dataset: 25, data: b"river".to_vec() }, // Keywords
//! ] };
//! let pm = IptcReader::new().read(Some(&iim), None)?;
//! assert_eq!(pm.city(), Some("Lyon"));
//! assert_eq!(pm.keywords(), ["river"]);
//! # Ok::<(), gamut_iptc::IptcError>(())
//! ```
//!
//! # Error contract
//!
//! **Strict write, honest read.** Writing never silently truncates or drops: a value that cannot
//! be encoded in the writer's charset, exceeds its dataset's maximum octet length, or (for
//! `photoshop:DateCreated`) is not an IIM-expressible ISO-8601 date-time is a hard
//! [`crate::IptcError::Malformed`]. Reading never guesses: a `1:90` coded-character-set
//! designation gamut does not support is a hard [`crate::IptcError::Unsupported`], not a Latin-1
//! fallback. Within a supported charset, an individual dataset value that fails to decode is
//! treated as absent — one corrupt value must not destroy access to the rest.
//!
//! # Scope
//!
//! gamut-iptc is the IPTC *semantics* layer. For the modern path it operates on an in-memory XMP
//! property graph ([`PhotoMetadata`] over [`gamut_xmp`], a public dependency re-exported as
//! [`xmp`]); parsing and serializing the XMP packet bytes is [`gamut_xmp`]'s responsibility (issue
//! #34). Exotic ISO 2022 character sets beyond Latin-1 and UTF-8 are reported as
//! [`crate::IptcError::Unsupported`] rather than mis-decoded (see [`charset`]). The typed
//! accessors cover every scalar/list IPTC **Core** property, plus the structured
//! `Iptc4xmpCore:CreatorContactInfo` and the most-used IPTC **Extension** structures — image
//! regions, artwork/object and licensors (see [`extension`]); the remaining Extension structures
//! pass through [`PhotoMetadata::xmp`] as raw values. Scalar-shaped IIM datasets that repeat on the
//! wire (`2:04`, `2:85`) reconcile their first value only; the IIM tag table names every dataset
//! IIM 4.2 gives a determinate octet maximum for — all of records 1 and 2 bar `2:202`, plus `7:10`
//! Size Mode — and everything else round-trips byte-exact without a name. See `STATUS.md` for the full deferral list.
#![forbid(unsafe_code)]

pub mod charset;
pub mod extension;
pub mod iim;
pub mod irb;
pub mod photo_metadata;
pub mod reader;
pub mod schema;
pub mod writer;

mod date;
mod error;
mod reconcile;

pub use charset::IimCharset;
pub use error::{IptcError, Result};
pub use extension::{
    ArtworkOrObject, CreatorContactInfo, Entity, ImageRegion, Licensor, RegionBoundary,
    RegionBoundaryPoint,
};
/// The XMP value model this crate's API speaks ([`XmpMeta`](gamut_xmp::XmpMeta),
/// [`XmpProperty`](gamut_xmp::XmpProperty), …).
///
/// [`gamut_xmp`] is a **public dependency**: [`PhotoMetadata`] holds its property graph and the
/// reader/writer take and return its types. This re-export names the exact matching version, so a
/// consumer never has to pin `gamut-xmp` separately.
pub use gamut_xmp as xmp;
pub use iim::{IimBlock, IimDataSet, IimFieldKind, IimTagInfo};
pub use irb::{IrbBlock, PhotoshopIrb};
pub use photo_metadata::PhotoMetadata;
pub use reader::{ConflictPolicy, FieldConflict, IptcReader};
pub use writer::IptcWriter;
