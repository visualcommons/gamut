//! `gamut-icc` — ICC color profile parsing and serialization.
//!
//! An ICC profile is the self-describing colour-characterization blob embedded in images (the WebP
//! `ICCP` chunk, the AVIF/HEIF `colr` box of type `prof`, a JPEG `APP2` segment): a 128-byte header,
//! a tag table, then the tag element data the table points at. It is a flat, offset-indexed binary
//! format that needs neither the TIFF/IFD machinery nor XML, so this crate's whole dependency list
//! is [`gamut_core`], [`gamut_color`] (the colorimetry behind the built-in profile constructors
//! below), `md-5` (for the §7.2.18 profile ID) and `thiserror`.
//!
//! Layouts follow **ICC.1:2022** (profile version 4.4, equivalent to ISO 15076-1; see
//! `references/icc`). Profile **v2** — still the most common version in real images — is supported,
//! including its legacy `textDescriptionType`.
//!
//! # Reading and writing
//!
//! [`IccProfile::parse`] decodes a profile and [`IccProfile::to_bytes`] re-serializes it; look tags
//! up with [`IccProfile::get`], optionally via the [`KnownTag`] catalogue. [`IccReader`] and
//! [`IccWriter`] carry options (strict parsing; profile-ID recomputation).
//! [`IccProfile::validate`] reports any ICC.1:2022 §8 required tags missing for the profile's
//! device class.
//!
//! # Built-in profiles
//!
//! [`IccProfile::builtin`] constructs a spec-valid v4 matrix/TRC profile for a named
//! [`BuiltinProfile`] space, [`IccProfile::gray_with_gamma`] the monochrome equivalent, and
//! [`IccProfile::from_cicp`] / [`IccProfile::from_source_profile`] the same from what a codec
//! actually signals — an H.273 code-point triple, or a [`gamut_color::SourceProfile`]. Each
//! returns `None` for signalling no matrix/TRC profile can describe. The colorimetry behind them
//! is [`gamut_color`]'s, never restated here; see `src/builtin.rs`.
//!
//! ```no_run
//! use gamut_icc::{IccProfile, KnownTag, TagData};
//!
//! # fn demo(bytes: &[u8]) -> Result<(), gamut_icc::IccError> {
//! let profile = IccProfile::parse(bytes)?;
//! if let Some(TagData::Xyz(white)) = profile.get(KnownTag::MediaWhitePoint) {
//!     println!("media white point: {:?}", white[0].to_f64());
//! }
//! let serialized = profile.to_bytes()?; // spec-valid bytes, ready to re-embed
//! # let _ = serialized;
//! # Ok(())
//! # }
//! ```
//!
//! # Scope
//!
//! Every ICC.1:2022 §10 element type is decoded semantically (see [`TagData`]). Any element type not
//! defined in §10 — iccMAX's `multiProcessElementsType`, or private/unregistered types — is
//! preserved verbatim as [`TagData::Raw`], so every profile round-trips losslessly regardless of
//! what it carries. Applying a profile's transform (a CMM), and building transforms from
//! [`gamut_color`], are out of scope — the `to_f64`/`eval` accessors are the seam for that — as is
//! **iccMAX** (`ICC.2`), a separate next-generation profile format. Note the distinction from the
//! built-in constructors above: this crate reads `gamut-color`'s colorimetry to *describe* a
//! colour space, and still applies nothing.
//!
//! # Stability
//!
//! The v1 surface follows a few deliberate policies:
//!
//! - **Plain-data model.** The tag structs expose public fields and are *not* `#[non_exhaustive]`:
//!   they mirror layouts ICC.1:2022 froze, so they will not grow fields. The two open sets —
//!   [`TagData`] (new semantic decoders) and [`KnownTag`] (catalogue entries) — are
//!   `#[non_exhaustive]` and grow additively.
//! - **Exhaustive enums mirror closed spec registries**: [`DeviceClass`] (§7.2.5), [`ColorSpace`]
//!   (§7.2.6, with [`ColorSpace::NColor`] covering the xCLR family), [`RenderingIntent`]
//!   (§7.2.15), [`ClutPrecision`], [`Curve`] (the three count-selected §10.6 encodings),
//!   [`CurveOrParametric`] and [`EmbeddedDescription`] (the only element types those slots admit).
//! - **The `tags` vec is the model.** [`IccProfile::tags`] keeps file order (the byte-fidelity
//!   contract for re-serialization); duplicate signatures are rejected at parse *and* write;
//!   [`IccProfile::get`] is a linear first-match scan, sized for the dozens of tags real profiles
//!   carry.
//! - **Writes are validated.** Serialization returns `Err` for hand-built data that violates an
//!   invariant the decoder establishes, instead of emitting a corrupt profile; a parsed profile
//!   always serializes. The format's `u32` sizes and offsets cap a serialized profile at 4 GiB.
#![forbid(unsafe_code)]

mod builtin;
mod bytes;
mod cicp;
mod colorant;
mod curve;
mod data;
mod dict;
mod error;
mod header;
mod lut;
mod measurement;
mod mluc;
mod named_color;
mod primitives;
mod profile;
mod reader;
mod sequence;
mod tag_types;
mod tags;
mod validate;
mod writer;

pub use builtin::BuiltinProfile;
pub use cicp::Cicp;
pub use colorant::{Colorant, ColorantOrder, ColorantTable};
pub use curve::{Curve, CurveOrParametric, ParametricCurve};
pub use data::DataElement;
pub use dict::{Dict, DictEntry};
pub use error::{IccError, Result};
pub use header::{
    ColorSpace, DeviceClass, ProfileHeader, ProfileId, ProfileVersion, RenderingIntent,
};
pub use lut::{Clut, ClutPrecision, Lut8, Lut16, LutAToB, LutBToA, Matrix3x3, Matrix3x4};
pub use measurement::{Chromaticity, Measurement, ViewingConditions};
pub use mluc::{Mluc, MlucRecord, TextDescription};
pub use named_color::{NamedColor, NamedColor2};
pub use primitives::{DateTime, S15Fixed16, Signature, U8Fixed8, U16Fixed16, XyzNumber};
pub use profile::IccProfile;
pub use reader::IccReader;
pub use sequence::{
    EmbeddedDescription, ProfileDescription, ProfileIdentifier, ProfileSequenceDesc,
    ProfileSequenceIdentifier, Response16, ResponseCurve, ResponseCurveSet16,
};
pub use tag_types::TagData;
pub use tags::KnownTag;
pub use validate::Conformance;
pub use writer::IccWriter;
