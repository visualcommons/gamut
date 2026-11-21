//! EXIF ↔ XMP mapping: [`exif_to_xmp`] and [`xmp_to_exif`], with an [`ExifConflictPolicy`].
//!
//! A file can carry the same datum twice — once as an EXIF tag, once as an XMP property in the
//! `tiff:`/`exif:` namespaces (or the `dc:`/`xmp:`/`photoshop:` properties the Metadata Working
//! Group assigns to the descriptive tags). These two functions derive one carrier from the other,
//! row by row over one table, the way IPTC-IIM is reconciled with XMP in [`crate::iptc`].
//!
//! # Which tags
//!
//! The rows are exiv2's EXIF ↔ XMP conversion table (`Converter::conversion_` in exiv2 0.28's
//! `convert.cpp`, the table behind `copyExifToXmp` / `copyXmpToExif`), whose date rows follow the
//! MWG guidelines: `DateTime` ↔ `xmp:ModifyDate`, `DateTimeOriginal` ↔ `photoshop:DateCreated`,
//! `DateTimeDigitized` ↔ `xmp:CreateDate`, and `ImageDescription`/`Artist`/`Copyright` ↔
//! `dc:description`/`dc:creator`/`dc:rights`. Six of exiv2's rows are left out because exiv2
//! writes them in a form CIPA DC-010 does not define: `OECF`, `SpatialFrequencyResponse`,
//! `CFAPattern` and `DeviceSettingDescription` are XMP *structures* in DC-010 §7.9, which exiv2
//! flattens to a list of byte values, and `GPSProcessingMethod` and `GPSAreaInformation` are text,
//! which exiv2 writes with their character-code prefix spelled into it. Their conversion, the
//! `exifEX:` (Exif 2.21+) properties DC-010 adds beyond exiv2's table, and the `OffsetTime*`
//! zones the dates could carry, are not implemented yet.
//!
//! # Value forms
//!
//! Each row converts the way exiv2's does — dates to ISO 8601 with the `SubSecTime*` fraction
//! folded in, `ExifVersion` to its four characters, `GPSVersionID` to `2.2.0.0`, `Flash` to the
//! `exif:Flash` structure, GPS coordinates to `DDD,MM.mmmmmmmR`, `UserComment` decoded from its
//! character code — with one deliberate difference, taken from CIPA DC-010 §4: a numeric tag with
//! more than one value maps to an ordered array with **one entry per value** (`tiff:BitsPerSample`
//! = `8`, `8`, `8`), where exiv2 writes a single entry holding the values space-separated. Reading
//! XMP back, an array entry is split on whitespace, so both forms are accepted.
//!
//! Two smaller differences, each pinned by the exiv2 differential:
//!
//! - Converting an XMP date to EXIF keeps the wall-clock time the XMP value states and drops its
//!   zone designator: EXIF's date tags carry local time without a zone. exiv2 instead moves the
//!   time into the zone of the machine running it. A fractional second becomes the nine-digit
//!   `SubSecTime*` value with its leading zeros (`.06` → `060000000`), where exiv2 drops them.
//! - Non-ASCII text goes to a text tag as Exif 3.0's `UTF8` type where the tag allows it
//!   (DC-008 Table 6: `ImageDescription`, `Make`, `Model`, `Software`, `Artist`, `Copyright`);
//!   exiv2 writes the same bytes under `ASCII`.
//!
//! As in exiv2's output, EXIF text ends at its first NUL, and an empty description, copyright or
//! comment maps to no property at all.
//!
//! # Conflicts
//!
//! A conversion never touches a target the source does not map. Where the source maps a tag and
//! the target already holds it, the [`ExifConflictPolicy`] decides: the winning carrier's value
//! overwrites, the losing carrier's is kept. With [`ExifConflictPolicy::ExifWins`],
//! [`exif_to_xmp`] is exiv2's `copyExifToXmp`; with the default
//! [`ExifConflictPolicy::XmpWins`], [`xmp_to_exif`] is exiv2's `copyXmpToExif`.
//!
//! ```
//! use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
//! use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
//! use gamut_metadata::{ExifConflictPolicy, exif_to_xmp, xmp_to_exif};
//!
//! let mut exif = Exif::new(ByteOrder::LittleEndian);
//! exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
//! exif.set_tag(ExifTag::DateTimeOriginal, Value::Ascii("2024:05:06 07:08:09".into()));
//!
//! let mut xmp = XmpMeta::new();
//! exif_to_xmp(&exif, &mut xmp, ExifConflictPolicy::ExifWins);
//! assert_eq!(xmp.get_text(WellKnownNs::Tiff.uri(), "Make"), Some("gamut"));
//! assert_eq!(
//!     xmp.get_text(WellKnownNs::Photoshop.uri(), "DateCreated"),
//!     Some("2024-05-06T07:08:09")
//! );
//!
//! // ...and back, into an empty EXIF.
//! let mut back = Exif::new(ByteOrder::LittleEndian);
//! xmp_to_exif(&xmp, &mut back, ExifConflictPolicy::XmpWins);
//! assert_eq!(back.datetime_original(), Some("2024:05:06 07:08:09"));
//! ```

use gamut_exif::{ByteOrder, Exif, ExifTag, FieldType, Value, as_text};
use gamut_xmp::{WellKnownNs, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};

/// Which carrier wins when EXIF and XMP both hold a mapped datum with differing values.
///
/// Shaped like [`iptc::ConflictPolicy`](crate::iptc::ConflictPolicy): the MWG guidelines and
/// CIPA DC-010 call for keeping the carriers in sync but name no single winner for a library to
/// apply, so this is gamut's explicit knob. Marked `#[non_exhaustive]`: finer-grained policies may
/// be added later. `#[repr(u8)]` with permanent, append-only discriminants, so the value crosses a
/// C boundary unchanged; a new policy takes the next free discriminant.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExifConflictPolicy {
    /// The XMP value wins (the default, as for IPTC: XMP is the modern carrier). [`exif_to_xmp`]
    /// fills only the XMP properties that are absent; [`xmp_to_exif`] overwrites.
    #[default]
    XmpWins = 0,
    /// The EXIF value wins. [`exif_to_xmp`] overwrites; [`xmp_to_exif`] fills only the EXIF tags
    /// that are absent.
    ExifWins = 1,
}

/// The XMP shape a plain row's value takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A simple text value.
    Text,
    /// An ordered array: one entry per numeric value, or one entry holding a text value.
    Seq,
    /// A language alternative with one `x-default` entry.
    LangAlt,
}

/// How a row converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conv {
    /// The value's text form, in the given XMP shape.
    Plain(Shape),
    /// An EXIF `YYYY:MM:DD HH:MM:SS` date (plus its `SubSecTime*` tag) ↔ an ISO 8601 date.
    Date(Option<ExifTag>),
    /// `ExifVersion`/`FlashpixVersion`: four ASCII digit bytes ↔ their text.
    Version,
    /// `GPSVersionID`: four bytes ↔ `a.b.c.d`.
    GpsVersion,
    /// `Flash` ↔ the `exif:Flash` structure.
    Flash,
    /// A GPS coordinate (plus its `…Ref` tag) ↔ `DDD,MM.mmmmmmmR`.
    GpsCoord(ExifTag),
    /// `UserComment` (character code + text) ↔ a language alternative.
    Comment,
    /// `GPSTimeStamp` (plus `GPSDateStamp`) ↔ an ISO 8601 date.
    GpsTime,
}

/// One row of the mapping: an EXIF tag, its XMP property, and how the two convert.
#[derive(Debug, Clone, Copy)]
struct Row {
    tag: ExifTag,
    ns: WellKnownNs,
    name: &'static str,
    conv: Conv,
}

/// Shorthand for a table row.
const fn row(tag: ExifTag, ns: WellKnownNs, name: &'static str, conv: Conv) -> Row {
    Row {
        tag,
        ns,
        name,
        conv,
    }
}

use Conv::{Comment, Date, Flash, GpsCoord, GpsTime, GpsVersion, Plain, Version};
use ExifTag as T;
use Shape::{LangAlt, Seq, Text};
use WellKnownNs::{DublinCore as Dc, Exif as ExifNs, Photoshop, Tiff, Xmp};

/// The mapping, in exiv2's `conversion_` order (see the module docs for the rows left out).
const ROWS: &[Row] = &[
    row(T::ImageWidth, Tiff, "ImageWidth", Plain(Text)),
    row(T::ImageLength, Tiff, "ImageLength", Plain(Text)),
    row(T::BitsPerSample, Tiff, "BitsPerSample", Plain(Seq)),
    row(T::Compression, Tiff, "Compression", Plain(Text)),
    row(
        T::PhotometricInterpretation,
        Tiff,
        "PhotometricInterpretation",
        Plain(Text),
    ),
    row(T::Orientation, Tiff, "Orientation", Plain(Text)),
    row(T::SamplesPerPixel, Tiff, "SamplesPerPixel", Plain(Text)),
    row(
        T::PlanarConfiguration,
        Tiff,
        "PlanarConfiguration",
        Plain(Text),
    ),
    row(T::YCbCrSubSampling, Tiff, "YCbCrSubSampling", Plain(Seq)),
    row(T::YCbCrPositioning, Tiff, "YCbCrPositioning", Plain(Text)),
    row(T::XResolution, Tiff, "XResolution", Plain(Text)),
    row(T::YResolution, Tiff, "YResolution", Plain(Text)),
    row(T::ResolutionUnit, Tiff, "ResolutionUnit", Plain(Text)),
    row(T::TransferFunction, Tiff, "TransferFunction", Plain(Seq)),
    row(T::WhitePoint, Tiff, "WhitePoint", Plain(Seq)),
    row(
        T::PrimaryChromaticities,
        Tiff,
        "PrimaryChromaticities",
        Plain(Seq),
    ),
    row(T::YCbCrCoefficients, Tiff, "YCbCrCoefficients", Plain(Seq)),
    row(
        T::ReferenceBlackWhite,
        Tiff,
        "ReferenceBlackWhite",
        Plain(Seq),
    ),
    row(T::DateTime, Xmp, "ModifyDate", Date(Some(T::SubSecTime))),
    row(T::ImageDescription, Dc, "description", Plain(LangAlt)),
    row(T::Make, Tiff, "Make", Plain(Text)),
    row(T::Model, Tiff, "Model", Plain(Text)),
    row(T::Software, Tiff, "Software", Plain(Text)),
    row(T::Artist, Dc, "creator", Plain(Seq)),
    row(T::Rating, Xmp, "Rating", Plain(Text)),
    row(T::Copyright, Dc, "rights", Plain(LangAlt)),
    row(T::ExifVersion, ExifNs, "ExifVersion", Version),
    row(T::FlashpixVersion, ExifNs, "FlashpixVersion", Version),
    row(T::ColorSpace, ExifNs, "ColorSpace", Plain(Text)),
    row(
        T::ComponentsConfiguration,
        ExifNs,
        "ComponentsConfiguration",
        Plain(Seq),
    ),
    row(
        T::CompressedBitsPerPixel,
        ExifNs,
        "CompressedBitsPerPixel",
        Plain(Text),
    ),
    row(T::PixelXDimension, ExifNs, "PixelXDimension", Plain(Text)),
    row(T::PixelYDimension, ExifNs, "PixelYDimension", Plain(Text)),
    row(T::UserComment, ExifNs, "UserComment", Comment),
    row(T::RelatedSoundFile, ExifNs, "RelatedSoundFile", Plain(Text)),
    row(
        T::DateTimeOriginal,
        Photoshop,
        "DateCreated",
        Date(Some(T::SubSecTimeOriginal)),
    ),
    row(
        T::DateTimeDigitized,
        Xmp,
        "CreateDate",
        Date(Some(T::SubSecTimeDigitized)),
    ),
    row(T::ExposureTime, ExifNs, "ExposureTime", Plain(Text)),
    row(T::FNumber, ExifNs, "FNumber", Plain(Text)),
    row(T::ExposureProgram, ExifNs, "ExposureProgram", Plain(Text)),
    row(
        T::SpectralSensitivity,
        ExifNs,
        "SpectralSensitivity",
        Plain(Text),
    ),
    row(
        T::PhotographicSensitivity,
        ExifNs,
        "ISOSpeedRatings",
        Plain(Seq),
    ),
    row(
        T::ShutterSpeedValue,
        ExifNs,
        "ShutterSpeedValue",
        Plain(Text),
    ),
    row(T::ApertureValue, ExifNs, "ApertureValue", Plain(Text)),
    row(T::BrightnessValue, ExifNs, "BrightnessValue", Plain(Text)),
    row(
        T::ExposureBiasValue,
        ExifNs,
        "ExposureBiasValue",
        Plain(Text),
    ),
    row(T::MaxApertureValue, ExifNs, "MaxApertureValue", Plain(Text)),
    row(T::SubjectDistance, ExifNs, "SubjectDistance", Plain(Text)),
    row(T::MeteringMode, ExifNs, "MeteringMode", Plain(Text)),
    row(T::LightSource, ExifNs, "LightSource", Plain(Text)),
    row(T::Flash, ExifNs, "Flash", Flash),
    row(T::FocalLength, ExifNs, "FocalLength", Plain(Text)),
    row(T::SubjectArea, ExifNs, "SubjectArea", Plain(Seq)),
    row(T::FlashEnergy, ExifNs, "FlashEnergy", Plain(Text)),
    row(
        T::FocalPlaneXResolution,
        ExifNs,
        "FocalPlaneXResolution",
        Plain(Text),
    ),
    row(
        T::FocalPlaneYResolution,
        ExifNs,
        "FocalPlaneYResolution",
        Plain(Text),
    ),
    row(
        T::FocalPlaneResolutionUnit,
        ExifNs,
        "FocalPlaneResolutionUnit",
        Plain(Text),
    ),
    row(T::SubjectLocation, ExifNs, "SubjectLocation", Plain(Seq)),
    row(T::ExposureIndex, ExifNs, "ExposureIndex", Plain(Text)),
    row(T::SensingMethod, ExifNs, "SensingMethod", Plain(Text)),
    row(T::FileSource, ExifNs, "FileSource", Plain(Text)),
    row(T::SceneType, ExifNs, "SceneType", Plain(Text)),
    row(T::CustomRendered, ExifNs, "CustomRendered", Plain(Text)),
    row(T::ExposureMode, ExifNs, "ExposureMode", Plain(Text)),
    row(T::WhiteBalance, ExifNs, "WhiteBalance", Plain(Text)),
    row(T::DigitalZoomRatio, ExifNs, "DigitalZoomRatio", Plain(Text)),
    row(
        T::FocalLengthIn35mmFilm,
        ExifNs,
        "FocalLengthIn35mmFilm",
        Plain(Text),
    ),
    row(T::SceneCaptureType, ExifNs, "SceneCaptureType", Plain(Text)),
    row(T::GainControl, ExifNs, "GainControl", Plain(Text)),
    row(T::Contrast, ExifNs, "Contrast", Plain(Text)),
    row(T::Saturation, ExifNs, "Saturation", Plain(Text)),
    row(T::Sharpness, ExifNs, "Sharpness", Plain(Text)),
    row(
        T::SubjectDistanceRange,
        ExifNs,
        "SubjectDistanceRange",
        Plain(Text),
    ),
    row(T::ImageUniqueId, ExifNs, "ImageUniqueID", Plain(Text)),
    row(T::GpsVersionId, ExifNs, "GPSVersionID", GpsVersion),
    row(
        T::GpsLatitude,
        ExifNs,
        "GPSLatitude",
        GpsCoord(T::GpsLatitudeRef),
    ),
    row(
        T::GpsLongitude,
        ExifNs,
        "GPSLongitude",
        GpsCoord(T::GpsLongitudeRef),
    ),
    row(T::GpsAltitudeRef, ExifNs, "GPSAltitudeRef", Plain(Text)),
    row(T::GpsAltitude, ExifNs, "GPSAltitude", Plain(Text)),
    row(T::GpsTimeStamp, ExifNs, "GPSTimeStamp", GpsTime),
    row(T::GpsSatellites, ExifNs, "GPSSatellites", Plain(Text)),
    row(T::GpsStatus, ExifNs, "GPSStatus", Plain(Text)),
    row(T::GpsMeasureMode, ExifNs, "GPSMeasureMode", Plain(Text)),
    row(T::GpsDop, ExifNs, "GPSDOP", Plain(Text)),
    row(T::GpsSpeedRef, ExifNs, "GPSSpeedRef", Plain(Text)),
    row(T::GpsSpeed, ExifNs, "GPSSpeed", Plain(Text)),
    row(T::GpsTrackRef, ExifNs, "GPSTrackRef", Plain(Text)),
    row(T::GpsTrack, ExifNs, "GPSTrack", Plain(Text)),
    row(
        T::GpsImgDirectionRef,
        ExifNs,
        "GPSImgDirectionRef",
        Plain(Text),
    ),
    row(T::GpsImgDirection, ExifNs, "GPSImgDirection", Plain(Text)),
    row(T::GpsMapDatum, ExifNs, "GPSMapDatum", Plain(Text)),
    row(
        T::GpsDestLatitude,
        ExifNs,
        "GPSDestLatitude",
        GpsCoord(T::GpsDestLatitudeRef),
    ),
    row(
        T::GpsDestLongitude,
        ExifNs,
        "GPSDestLongitude",
        GpsCoord(T::GpsDestLongitudeRef),
    ),
    row(
        T::GpsDestBearingRef,
        ExifNs,
        "GPSDestBearingRef",
        Plain(Text),
    ),
    row(T::GpsDestBearing, ExifNs, "GPSDestBearing", Plain(Text)),
    row(
        T::GpsDestDistanceRef,
        ExifNs,
        "GPSDestDistanceRef",
        Plain(Text),
    ),
    row(T::GpsDestDistance, ExifNs, "GPSDestDistance", Plain(Text)),
    row(T::GpsDifferential, ExifNs, "GPSDifferential", Plain(Text)),
];

/// Writes the XMP form of every mapped EXIF tag `exif` holds into `xmp`.
///
/// A property already in `xmp` is overwritten only under [`ExifConflictPolicy::ExifWins`]; a tag
/// whose value cannot be converted (a date that does not parse, a GPS coordinate without its
/// reference, …) is skipped. Properties outside the mapping are never touched.
pub fn exif_to_xmp(exif: &Exif, xmp: &mut XmpMeta, policy: ExifConflictPolicy) {
    for row in ROWS {
        let ns = row.ns.uri();
        if policy == ExifConflictPolicy::XmpWins && xmp.get(ns, row.name).is_some() {
            continue;
        }
        if let Some(value) = to_xmp(exif, row) {
            xmp.set(XmpProperty::new(ns, row.name, value));
        }
    }
}

/// Writes the EXIF form of every mapped XMP property `xmp` holds into `exif`.
///
/// A tag already in `exif` is overwritten only under [`ExifConflictPolicy::XmpWins`]; a property
/// whose value cannot be converted is skipped. The companion tags a conversion writes alongside
/// its target — `SubSecTime*` beside a date, `GPSDateStamp` beside `GPSTimeStamp`, the `…Ref` beside
/// a GPS coordinate — follow their target. Tags outside the mapping are never touched.
pub fn xmp_to_exif(xmp: &XmpMeta, exif: &mut Exif, policy: ExifConflictPolicy) {
    for row in ROWS {
        if policy == ExifConflictPolicy::ExifWins && exif.get_tag(row.tag).is_some() {
            continue;
        }
        let Some(prop) = xmp.get(row.ns.uri(), row.name) else {
            continue;
        };
        for (tag, value) in to_exif(prop, row, exif.byte_order()) {
            exif.set_tag(tag, value);
        }
    }
}

// ---- EXIF → XMP -------------------------------------------------------------------------------

/// The XMP value of `row`'s tag in `exif`, or `None` when it is absent or does not convert.
fn to_xmp(exif: &Exif, row: &Row) -> Option<XmpValue> {
    let value = exif.get_tag(row.tag)?;
    match row.conv {
        Plain(shape) => plain_to_xmp(value, shape),
        Date(subsec) => date_to_xmp(exif, value, subsec).map(XmpValue::Simple),
        Version => version_to_xmp(value).map(XmpValue::Simple),
        GpsVersion => Some(XmpValue::Simple(join(&numbers(value)?, "."))),
        Flash => flash_to_xmp(value),
        GpsCoord(reference) => coord_to_xmp(value, exif.get_tag(reference)?).map(XmpValue::Simple),
        // An empty comment (the all-NUL field cameras reserve) is no comment: exiv2 writes none.
        Comment => comment_to_xmp(value, exif.byte_order())
            .filter(|text| !text.is_empty())
            .map(|text| lang_alt(&text)),
        GpsTime => gps_time_to_xmp(exif, value).map(XmpValue::Simple),
    }
}

/// A plain value in `shape`: its text, its per-value entries, or an `x-default` alternative.
fn plain_to_xmp(value: &Value, shape: Shape) -> Option<XmpValue> {
    let parts = match as_text(value) {
        // Text ends at the first NUL: what follows is padding a writer reserved (`Software` of
        // a fixed width), not text.
        Some(text) => vec![text.split('\0').next().unwrap_or_default().to_owned()],
        None => numbers(value)?,
    };
    Some(match shape {
        Text => XmpValue::Simple(join(&parts, " ")),
        // An empty description or copyright notice says nothing; XMP has no empty alternative.
        LangAlt => Some(join(&parts, " "))
            .filter(|text| !text.is_empty())
            .map(|text| lang_alt(&text))?,
        Seq => XmpValue::Array(XmpArray::Seq(
            parts
                .into_iter()
                .filter(|part| !part.is_empty())
                .map(XmpItem::simple)
                .collect(),
        )),
    })
}

/// A one-entry `x-default` language alternative.
fn lang_alt(text: &str) -> XmpValue {
    XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text("x-default", text)]))
}

/// Joins `parts` with `sep`.
fn join(parts: &[String], sep: &str) -> String {
    parts.join(sep)
}

/// Each component of a numeric (or byte) value as exiv2 writes it: integers in decimal, rationals
/// as `num/den`. `None` for text and for the value kinds no mapped tag holds.
fn numbers(value: &Value) -> Option<Vec<String>> {
    fn each<V: ToString>(values: &[V]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }
    Some(match value {
        Value::Byte(v) | Value::Undefined(v) => each(v),
        Value::Short(v) => each(v),
        Value::Long(v) => each(v),
        Value::SByte(v) => each(v),
        Value::SShort(v) => each(v),
        Value::SLong(v) => each(v),
        Value::Rational(v) => v.iter().map(|(n, d)| format!("{n}/{d}")).collect(),
        Value::SRational(v) => v.iter().map(|(n, d)| format!("{n}/{d}")).collect(),
        _ => return None,
    })
}

/// The first component of an integer value, widened.
fn first_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Byte(v) | Value::Undefined(v) => v.first().map(|&x| i64::from(x)),
        Value::Short(v) => v.first().map(|&x| i64::from(x)),
        Value::Long(v) => v.first().map(|&x| i64::from(x)),
        Value::SShort(v) => v.first().map(|&x| i64::from(x)),
        Value::SLong(v) => v.first().map(|&x| i64::from(x)),
        _ => None,
    }
}

/// `ExifVersion`/`FlashpixVersion`: each byte as a character (`0x30 0x32 0x33 0x32` → `0232`).
fn version_to_xmp(value: &Value) -> Option<String> {
    match value {
        Value::Undefined(bytes) | Value::Byte(bytes) => {
            Some(bytes.iter().map(|&b| char::from(b)).collect())
        }
        _ => None,
    }
}

/// `Flash` → the `exif:Flash` structure (DC-010 §7.9 / XMP Part 2): `Fired`, `Return`, `Mode`,
/// `Function` and `RedEyeMode` unpacked from the bit field.
fn flash_to_xmp(value: &Value) -> Option<XmpValue> {
    let bits = first_integer(value)?;
    let ns = ExifNs.uri();
    let field = |name: &str, text: String| XmpProperty::new(ns, name, XmpValue::Simple(text));
    let flag = |bit: i64| {
        if bits >> bit & 1 == 1 {
            "True"
        } else {
            "False"
        }
        .to_owned()
    };
    Some(XmpValue::Structured(vec![
        field("Fired", flag(0)),
        field("Return", (bits >> 1 & 3).to_string()),
        field("Mode", (bits >> 3 & 3).to_string()),
        field("Function", flag(5)),
        field("RedEyeMode", flag(6)),
    ]))
}

/// A GPS coordinate (three rationals: degrees, minutes, seconds) and its reference → the XMP
/// `GPSCoordinate` form `DDD,MM.mmmmmmmR` (XMP Part 2 / DC-010 §8.3): whole degrees, decimal
/// minutes to seven places, and the reference letter.
fn coord_to_xmp(value: &Value, reference: &Value) -> Option<String> {
    let Value::Rational(parts) = value else {
        return None;
    };
    let [deg, min, sec] = parts.as_slice() else {
        return None;
    };
    let quotient = |&(n, d): &(u32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
    let mut minutes = quotient(deg)? * 60.0 + quotient(min)? + quotient(sec)? / 60.0;
    // Truncating toward zero, as exiv2's `static_cast<int>` does.
    let degrees = (minutes / 60.0) as i64;
    minutes -= (degrees * 60) as f64;
    let letter = as_text(reference)?.chars().next()?;
    Some(format!("{degrees},{minutes:.7}{letter}"))
}

/// `UserComment`: the eight-byte character code, then the text (Exif 3.0 §4.6.5 Table 9 / DC-008
/// `UserComment`). `UNICODE` text is UCS-2 in the stream's byte order unless a byte-order mark
/// says otherwise; `ASCII` and undefined text end at the first NUL; `JIS` text is passed through
/// as bytes, which decode only when they happen to be valid UTF-8.
fn comment_to_xmp(value: &Value, order: ByteOrder) -> Option<String> {
    let Value::Undefined(bytes) = value else {
        return None;
    };
    let (code, body) = bytes.split_at_checked(8)?;
    if code == b"UNICODE\0" {
        return utf16_to_string(body, order);
    }
    let body = if code == b"ASCII\0\0\0" || code == [0; 8] {
        body.split(|&b| b == 0).next().unwrap_or_default()
    } else {
        body
    };
    String::from_utf8(body.to_vec()).ok()
}

/// Decodes UCS-2/UTF-16 text, honouring a leading byte-order mark (and dropping it), else `order`.
fn utf16_to_string(body: &[u8], order: ByteOrder) -> Option<String> {
    if let Some(rest) = body.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8(rest.to_vec()).ok();
    }
    let (body, little) = match body {
        [0xFF, 0xFE, rest @ ..] => (rest, true),
        [0xFE, 0xFF, rest @ ..] => (rest, false),
        _ => (body, order == ByteOrder::LittleEndian),
    };
    let units = body.as_chunks::<2>().0.iter().map(|&pair| {
        if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// An EXIF date (`YYYY:MM:DD HH:MM:SS`) → ISO 8601 (`YYYY-MM-DDTHH:MM:SS`), with the companion
/// `SubSecTime*` digits appended as the fraction when they are a number.
fn date_to_xmp(exif: &Exif, value: &Value, subsec: Option<ExifTag>) -> Option<String> {
    let [year, month, day, hour, min, sec] = scan_ints(as_text(value)?, b":: ::")?;
    let fraction = subsec
        .and_then(|tag| exif.get_tag(tag))
        .and_then(subsec_fraction)
        .unwrap_or_default();
    Some(iso_date(
        [year, month, day, hour, min, sec],
        truncate_fraction(fraction),
    ))
}

/// A `SubSecTime*` value as the `.digits` fraction exiv2 appends: only an ASCII value that reads
/// as a whole number qualifies.
fn subsec_fraction(value: &Value) -> Option<String> {
    let Value::Ascii(text) = value else {
        return None;
    };
    parse_long(text)?;
    Some(format!(".{text}"))
}

/// Caps a fraction (with its leading `.`) at nine digits, as exiv2 does.
fn truncate_fraction(mut fraction: String) -> String {
    fraction.truncate(10);
    fraction
}

/// `YYYY-MM-DDTHH:MM:SS` followed by `fraction` (empty, or `.digits`).
fn iso_date([year, month, day, hour, min, sec]: [i64; 6], fraction: String) -> String {
    format!("{year:4}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}{fraction}")
}

/// `GPSTimeStamp` (hours, minutes, seconds as rationals, UTC) on the date of `GPSDateStamp` —
/// or, failing that, of `DateTimeOriginal` or `DateTimeDigitized` — → ISO 8601, with the
/// fractional second always written to nine places (exiv2's form).
fn gps_time_to_xmp(exif: &Exif, value: &Value) -> Option<String> {
    let Value::Rational(parts) = value else {
        return None;
    };
    let [hour, min, sec] = parts.as_slice() else {
        return None;
    };
    if parts.iter().any(|&(_, d)| d == 0) {
        return None;
    }
    // exiv2 reads hours and minutes through `float` and the seconds through `double`; the same
    // widths keep the rounding of a fractional second identical.
    let single = |&(n, d): &(u32, u32)| f64::from(n as f32 / d as f32);
    let mut seconds =
        single(hour) * 3600.0 + single(min) * 60.0 + f64::from(sec.0) / f64::from(sec.1);
    // Whole hours, then minutes, then seconds, each truncated toward zero as exiv2's
    // `static_cast<int>` does.
    let mut take = |unit: f64| {
        let whole = (seconds / unit) as i64;
        seconds -= (whole as f64) * unit;
        whole
    };
    let (h, m, s) = (take(3600.0), take(60.0), take(1.0));
    let fraction = format!("{seconds:.9}");
    let fraction = fraction.get(1..).unwrap_or_default().to_owned();
    let date = [T::GpsDateStamp, T::DateTimeOriginal, T::DateTimeDigitized]
        .into_iter()
        .find_map(|tag| exif.get_tag(tag))?;
    let [year, month, day] = scan_ints(as_text(date)?, b"::")?;
    Some(iso_date(
        [year, month, day, h, m, s],
        truncate_fraction(fraction),
    ))
}

/// Reads `N` integers separated by `seps` (`N - 1` bytes) the way C's `sscanf("%d:%d…")` does:
/// each integer may have leading whitespace and a sign, a `' '` separator matches any run of
/// whitespace (including none), any other separator must match exactly, and whatever follows the
/// last integer is ignored.
fn scan_ints<const N: usize>(text: &str, seps: &[u8]) -> Option<[i64; N]> {
    let mut out = [0; N];
    let mut rest = text;
    for (i, slot) in out.iter_mut().enumerate() {
        let (value, tail) = scan_int(rest)?;
        *slot = value;
        rest = tail;
        if let Some(&sep) = seps.get(i) {
            rest = if sep == b' ' {
                rest.trim_start()
            } else {
                rest.strip_prefix(char::from(sep))?
            };
        }
    }
    Some(out)
}

/// One `%d`: optional leading whitespace, an optional sign, then at least one digit.
fn scan_int(text: &str) -> Option<(i64, &str)> {
    let text = text.trim_start();
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let digits = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    let end = text.len() - unsigned.len() + digits;
    if digits == 0 {
        return None;
    }
    Some((text[..end].parse().ok()?, &text[end..]))
}

/// A whole number the way C++ `istream >> long` reads one when nothing but whitespace may follow.
fn parse_long(text: &str) -> Option<i64> {
    let (value, rest) = scan_int(text)?;
    rest.trim().is_empty().then_some(value)
}

// ---- XMP → EXIF -------------------------------------------------------------------------------

/// The EXIF tags `prop` converts to under `row`: the target first, then any companion tags.
/// Empty when the value does not convert.
fn to_exif(prop: &XmpProperty, row: &Row, order: ByteOrder) -> Vec<(ExifTag, Value)> {
    let value = match row.conv {
        Plain(_) => plain_to_exif(prop, row.tag),
        Date(subsec) => return date_to_exif(prop, row.tag, subsec),
        Version => version_to_exif(prop),
        GpsVersion => prop
            .text()
            .and_then(|text| bytes_of(text.replace('.', " ").split_whitespace()))
            .map(Value::Byte),
        Flash => flash_to_exif(prop),
        GpsCoord(reference) => return coord_to_exif(prop, row.tag, reference),
        Comment => xmp_text(prop).map(|text| comment_to_exif(&text, order)),
        GpsTime => return gps_time_to_exif(prop),
    };
    value
        .map(|value| vec![(row.tag, value)])
        .unwrap_or_default()
}

/// The text of a property the way exiv2 reads one: a simple value's text; a language
/// alternative's `x-default` entry (or its only entry); an array's entries joined with `", "`.
fn xmp_text(prop: &XmpProperty) -> Option<String> {
    match &prop.value {
        XmpValue::Simple(text) | XmpValue::Uri(text) => Some(text.clone()),
        XmpValue::Array(XmpArray::Alt(items)) => {
            let default = items.iter().find(|item| item.lang() == Some("x-default"));
            match (default, items.as_slice()) {
                (Some(item), _) | (None, [item]) => item.text().map(str::to_owned),
                (None, _) => None,
            }
        }
        XmpValue::Array(array) => {
            let texts: Vec<&str> = array.texts().collect();
            Some(texts.join(", "))
        }
        XmpValue::Structured(_) => None,
    }
}

/// The whitespace-separated tokens of a numeric property: every array entry is split, so both
/// DC-010's one-entry-per-value form and exiv2's single space-separated entry read the same.
fn tokens(prop: &XmpProperty) -> Option<Vec<String>> {
    let text = match &prop.value {
        XmpValue::Array(XmpArray::Seq(items) | XmpArray::Bag(items)) => {
            let texts: Vec<&str> = items.iter().filter_map(XmpItem::text).collect();
            texts.join(" ")
        }
        _ => xmp_text(prop)?,
    };
    Some(text.split_whitespace().map(str::to_owned).collect())
}

/// A plain property as a value of the type `tag` takes: text tags as `ASCII` (or `UTF8` where the
/// tag allows it and the text is not ASCII), numeric tags parsed token by token. `None` when a
/// token does not parse in that type.
fn plain_to_exif(prop: &XmpProperty, tag: ExifTag) -> Option<Value> {
    let types = tag.field_types();
    if types.contains(&FieldType::Ascii) {
        let text = xmp_text(prop)?;
        let utf8 = !text.is_ascii() && types.contains(&FieldType::Utf8);
        return Some(if utf8 {
            Value::Utf8(text)
        } else {
            Value::Ascii(text)
        });
    }
    let tokens = tokens(prop)?;
    if tokens.is_empty() {
        return None;
    }
    let tokens = tokens.iter().map(String::as_str);
    // `Rating` is catalogued from outside DC-008 with no mandated type; it is a SHORT (the type
    // exiv2 and the Microsoft photo tags give it).
    match types.first().copied().unwrap_or(FieldType::Short) {
        FieldType::Byte => bytes_of(tokens).map(Value::Byte),
        FieldType::Undefined => bytes_of(tokens).map(Value::Undefined),
        FieldType::Rational => tokens
            .map(parse_rational)
            .collect::<Option<_>>()
            .map(Value::Rational),
        FieldType::SRational => tokens
            .map(parse_srational)
            .collect::<Option<_>>()
            .map(Value::SRational),
        FieldType::Long => tokens
            .map(|t| t.parse().ok())
            .collect::<Option<_>>()
            .map(Value::Long),
        _ => short_or_long(tokens, types.contains(&FieldType::Long)),
    }
}

/// Integers as `SHORT`, or as `LONG` when one does not fit a `SHORT` and the tag allows `LONG`.
fn short_or_long<'a>(tokens: impl Iterator<Item = &'a str>, long: bool) -> Option<Value> {
    let values: Vec<u32> = tokens.map(|t| t.parse().ok()).collect::<Option<_>>()?;
    let shorts: Option<Vec<u16>> = values.iter().map(|&v| u16::try_from(v).ok()).collect();
    match shorts {
        Some(shorts) => Some(Value::Short(shorts)),
        None if long => Some(Value::Long(values)),
        None => None,
    }
}

/// Byte tokens (`0`–`255`).
fn bytes_of<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<Vec<u8>> {
    tokens.map(|t| t.parse().ok()).collect()
}

/// An unsigned rational written `num/den`.
fn parse_rational(token: &str) -> Option<(u32, u32)> {
    let (n, d) = token.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?))
}

/// A signed rational written `num/den`.
fn parse_srational(token: &str) -> Option<(i32, i32)> {
    let (n, d) = token.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?))
}

/// `exif:ExifVersion`/`exif:FlashpixVersion`: the first four characters as bytes.
fn version_to_exif(prop: &XmpProperty) -> Option<Value> {
    let text = prop.text()?;
    let bytes = text.as_bytes().get(..4)?;
    Some(Value::Undefined(bytes.to_vec()))
}

/// The `exif:Flash` structure → the `Flash` bit field. Requires `Fired`; a missing or unreadable
/// other field contributes nothing.
fn flash_to_exif(prop: &XmpProperty) -> Option<Value> {
    let XmpValue::Structured(fields) = &prop.value else {
        return None;
    };
    let ns = ExifNs.uri();
    let field = |name: &str| {
        fields
            .iter()
            .find(|f| f.namespace == ns && f.name == name)
            .and_then(XmpProperty::text)
            .and_then(parse_flag)
    };
    if !fields
        .iter()
        .any(|f| f.namespace == ns && f.name == "Fired")
    {
        return None;
    }
    let mut bits = 0;
    for (name, mask, shift) in [
        ("Fired", 1, 0),
        ("Return", 3, 1),
        ("Mode", 3, 3),
        ("Function", 1, 5),
        ("RedEyeMode", 1, 6),
    ] {
        if let Some(v) = field(name) {
            bits |= (v & mask) << shift;
        }
    }
    Some(Value::Short(vec![bits]))
}

/// A `Flash` field: a whole number, or `True`/`False` (any case).
fn parse_flag(text: &str) -> Option<u16> {
    if let Some(v) = parse_long(text) {
        return u16::try_from(v & 0xFF).ok();
    }
    match text.trim().to_ascii_lowercase().as_str() {
        "true" => Some(1),
        "false" => Some(0),
        _ => None,
    }
}

/// `UserComment` text → the `UNICODE` character code and UCS-2 text in the stream's byte order
/// (exiv2's choice, which no text can fail).
fn comment_to_exif(text: &str, order: ByteOrder) -> Value {
    let mut bytes = b"UNICODE\0".to_vec();
    for unit in text.encode_utf16() {
        let pair = if order == ByteOrder::LittleEndian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&pair);
    }
    Value::Undefined(bytes)
}

/// The XMP `GPSCoordinate` form `DDD,MM,SSk` or `DDD,MM.mmk` → three rationals plus the `…Ref`
/// letter.
fn coord_to_exif(prop: &XmpProperty, tag: ExifTag, reference: ExifTag) -> Vec<(ExifTag, Value)> {
    let parse = || -> Option<Vec<(ExifTag, Value)>> {
        let text = prop.text()?;
        let letter = text
            .chars()
            .last()
            .filter(|c| matches!(c, 'N' | 'S' | 'E' | 'W'))?;
        let body = &text[..text.len() - 1];
        let parts: Vec<f64> = body
            .split(',')
            .map(|p| p.trim().parse().ok())
            .collect::<Option<_>>()?;
        let (deg, min, sec) = match *parts.as_slice() {
            [deg, min] => (deg, min.trunc(), min.fract() * 60.0),
            [deg, min, sec] => (deg, min, sec),
            _ => return None,
        };
        let rationals = [deg, min, sec]
            .into_iter()
            .map(float_to_rational)
            .collect::<Option<_>>()?;
        Some(vec![
            (tag, Value::Rational(rationals)),
            (reference, Value::Ascii(letter.to_string())),
        ])
    };
    parse().unwrap_or_default()
}

/// exiv2's `floatToRationalCast`: the value rounded to single precision, then over a power-of-ten
/// denominator as large as keeps the numerator in an `i32`, reduced. Negative and out-of-range
/// values do not fit an unsigned rational.
fn float_to_rational(value: f64) -> Option<(u32, u32)> {
    let d = f64::from(value as f32);
    let den: u32 = match d.abs() {
        a if a <= 2_147.0 => 1_000_000,
        a if a <= 214_748.0 => 10_000,
        a if a <= 21_474_836.0 => 100,
        a if a <= 2_147_483_647.0 => 1,
        _ => return None,
    };
    // Rounded half away from zero, as `std::lround`; the denominator keeps it within an `i32`.
    let num = u32::try_from((d * f64::from(den)).round() as i64).ok()?;
    let g = gcd(num, den);
    Some((num / g, den / g))
}

/// Greatest common divisor (`den` is never zero, so neither is the result).
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A date in the XMP `Date` form (Part 1 §8.2.1.1): `YYYY[-MM[-DD[Thh:mm[:ss[.s…]][TZD]]]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct XmpDate {
    fields: [i64; 6],
    /// The fractional second's digits, at most nine, as written (empty when there are none).
    fraction: String,
}

impl XmpDate {
    /// The fraction as nanoseconds (its digits right-padded to nine).
    fn nanos(&self) -> u32 {
        format!("{:0<9}", self.fraction).parse().unwrap_or_default()
    }
}

/// Parses an XMP date; the zone designator is checked for form and then dropped.
fn parse_xmp_date(text: &str) -> Option<XmpDate> {
    let text = text.trim();
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut fields = [0i64; 6];
    let mut date_parts = date.split('-');
    for (slot, width) in fields.iter_mut().zip([4, 2, 2]) {
        let Some(part) = date_parts.next() else { break };
        *slot = fixed_digits(part, width)?;
    }
    if date_parts.next().is_some() {
        return None;
    }
    let mut fraction = String::new();
    if let Some(time) = time {
        let zone_at = time.find(['Z', '+', '-']).unwrap_or(time.len());
        let (clock, zone) = time.split_at(zone_at);
        check_zone(zone)?;
        let (clock, frac) = match clock.split_once('.') {
            Some((clock, frac)) => (clock, Some(frac)),
            None => (clock, None),
        };
        let mut clock_parts = clock.split(':');
        for slot in &mut fields[3..] {
            let Some(part) = clock_parts.next() else {
                break;
            };
            *slot = fixed_digits(part, 2)?;
        }
        if clock_parts.next().is_some() || !clock.contains(':') {
            return None;
        }
        if let Some(frac) = frac {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            fraction = frac.chars().take(9).collect();
        }
    }
    Some(XmpDate { fields, fraction })
}

/// Exactly `width` ASCII digits.
fn fixed_digits(text: &str, width: usize) -> Option<i64> {
    (text.len() == width && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// An empty zone, `Z`, or `±hh:mm`.
fn check_zone(zone: &str) -> Option<()> {
    match zone.as_bytes() {
        [] | [b'Z'] => Some(()),
        [b'+' | b'-', ..] => {
            let (h, m) = zone[1..].split_once(':')?;
            fixed_digits(h, 2)?;
            fixed_digits(m, 2)?;
            Some(())
        }
        _ => None,
    }
}

/// An XMP date → the EXIF `YYYY:MM:DD HH:MM:SS` form, plus the nine-digit `SubSecTime*` value
/// when the fraction is non-zero.
fn date_to_exif(
    prop: &XmpProperty,
    tag: ExifTag,
    subsec: Option<ExifTag>,
) -> Vec<(ExifTag, Value)> {
    let Some(date) = prop.text().and_then(parse_xmp_date) else {
        return Vec::new();
    };
    let [year, month, day, hour, min, sec] = date.fields;
    let text = format!("{year:4}:{month:02}:{day:02} {hour:02}:{min:02}:{sec:02}");
    let mut out = vec![(tag, Value::Ascii(text))];
    let nanos = date.nanos();
    if let Some(subsec) = subsec
        && nanos != 0
    {
        out.push((subsec, Value::Ascii(format!("{nanos:09}"))));
    }
    out
}

/// An XMP date → `GPSTimeStamp` (hours, minutes, seconds; a fractional second moves the whole
/// seconds into the minutes so the nanoseconds fit the seconds' rational) plus `GPSDateStamp`.
/// The zone is ignored: the XMP value is taken to be UTC, as `GPSTimeStamp` is.
fn gps_time_to_exif(prop: &XmpProperty) -> Vec<(ExifTag, Value)> {
    let Some(date) = prop.text().and_then(parse_xmp_date) else {
        return Vec::new();
    };
    let [year, month, day, hour, min, sec] = date.fields;
    let narrow = |v: i64| u32::try_from(v).unwrap_or_default();
    let (hour, min, sec) = (narrow(hour), narrow(min), narrow(sec));
    let nanos = date.nanos();
    let (rmin, rsec) = match (nanos, sec) {
        (0, _) => ((min, 1), (sec, 1)),
        (_, 0) => ((min, 1), (nanos, 1_000_000_000)),
        _ => ((min * 60 + sec, 60), (nanos, 1_000_000_000)),
    };
    vec![
        (
            T::GpsTimeStamp,
            Value::Rational(vec![(hour, 1), rmin, rsec]),
        ),
        (
            T::GpsDateStamp,
            Value::Ascii(format!("{year:4}:{month:02}:{day:02}")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exif() -> Exif {
        Exif::new(ByteOrder::LittleEndian)
    }

    fn ascii(text: &str) -> Value {
        Value::Ascii(text.to_owned())
    }

    fn row_for(tag: ExifTag) -> &'static Row {
        ROWS.iter().find(|r| r.tag == tag).unwrap()
    }

    #[test]
    fn every_row_maps_a_distinct_tag_and_a_distinct_property() {
        for (i, a) in ROWS.iter().enumerate() {
            for b in &ROWS[i + 1..] {
                assert_ne!(a.tag, b.tag);
                assert_ne!((a.ns, a.name), (b.ns, b.name));
            }
        }
    }

    #[test]
    fn exif_wins_overwrites_and_xmp_wins_keeps_an_existing_property() {
        let mut source = exif();
        source.set_tag(T::Make, ascii("Canon"));
        let tiff = Tiff.uri();

        let mut xmp = XmpMeta::new();
        xmp.set_text(tiff, "Make", "Nikon");
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::XmpWins);
        assert_eq!(xmp.get_text(tiff, "Make"), Some("Nikon"));
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(xmp.get_text(tiff, "Make"), Some("Canon"));
    }

    #[test]
    fn xmp_wins_overwrites_and_exif_wins_keeps_an_existing_tag() {
        let mut xmp = XmpMeta::new();
        xmp.set_text(Tiff.uri(), "Make", "Canon");

        let mut target = exif();
        target.set_tag(T::Make, ascii("Nikon"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::ExifWins);
        assert_eq!(target.make(), Some("Nikon"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.make(), Some("Canon"));
    }

    #[test]
    fn the_default_policy_is_xmp_wins_with_discriminants_zero_and_one() {
        assert_eq!(ExifConflictPolicy::default(), ExifConflictPolicy::XmpWins);
        assert_eq!(ExifConflictPolicy::XmpWins as u8, 0);
        assert_eq!(ExifConflictPolicy::ExifWins as u8, 1);
    }

    #[test]
    fn a_multi_valued_tag_maps_to_one_seq_entry_per_value() {
        let value = plain_to_xmp(&Value::Short(vec![8, 8, 8]), Seq).unwrap();
        let XmpValue::Array(XmpArray::Seq(items)) = value else {
            panic!("not a Seq: {value:?}")
        };
        assert_eq!(
            items.iter().filter_map(XmpItem::text).collect::<Vec<_>>(),
            ["8", "8", "8"]
        );
    }

    #[test]
    fn a_text_tag_maps_to_one_seq_entry_and_rationals_to_num_over_den() {
        let creator = plain_to_xmp(&ascii("A, B"), Seq).unwrap();
        assert_eq!(
            creator,
            XmpValue::Array(XmpArray::Seq(vec![XmpItem::simple("A, B")]))
        );
        let text = plain_to_xmp(&Value::SRational(vec![(-1, 3), (2, 5)]), Text).unwrap();
        assert_eq!(text, XmpValue::Simple("-1/3 2/5".into()));
        assert_eq!(plain_to_xmp(&Value::Float(vec![1.0]), Text), None);
    }

    #[test]
    fn text_ends_at_the_first_nul_and_empty_text_maps_to_no_entry() {
        assert_eq!(
            plain_to_xmp(&ascii("DC-4300\0\0junk"), Text),
            Some(XmpValue::Simple("DC-4300".into()))
        );
        assert_eq!(plain_to_xmp(&ascii(""), LangAlt), None);
        assert_eq!(
            plain_to_xmp(&ascii("\0"), Seq),
            Some(XmpValue::Array(XmpArray::Seq(Vec::new())))
        );
        assert_eq!(
            plain_to_xmp(&ascii(""), Text),
            Some(XmpValue::Simple(String::new()))
        );
    }

    #[test]
    fn an_empty_user_comment_maps_to_no_property() {
        let mut source = exif();
        source.set_tag(T::UserComment, Value::Undefined(vec![0; 16]));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(xmp.get(ExifNs.uri(), "UserComment"), None);
        source.set_tag(T::UserComment, Value::Undefined(b"ASCII\0\0\0hi".to_vec()));
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(
            xmp.get_lang_alt(ExifNs.uri(), "UserComment", "x-default"),
            Some("hi")
        );
    }

    #[test]
    fn numbers_renders_every_integer_kind_in_decimal() {
        let cases = [
            (Value::Byte(vec![1, 255]), "1 255"),
            (Value::Undefined(vec![3]), "3"),
            (Value::Long(vec![70_000]), "70000"),
            (Value::SByte(vec![-1]), "-1"),
            (Value::SShort(vec![-2]), "-2"),
            (Value::SLong(vec![-3]), "-3"),
            (Value::Rational(vec![(72, 1)]), "72/1"),
        ];
        for (value, text) in cases {
            assert_eq!(numbers(&value).unwrap().join(" "), text, "{value:?}");
        }
    }

    #[test]
    fn version_bytes_become_characters() {
        assert_eq!(
            version_to_xmp(&Value::Undefined(b"0232".to_vec())).as_deref(),
            Some("0232")
        );
        assert_eq!(
            version_to_xmp(&Value::Byte(b"0100".to_vec())).as_deref(),
            Some("0100")
        );
        assert_eq!(version_to_xmp(&ascii("0232")), None);
    }

    #[test]
    fn flash_unpacks_each_field_of_the_bit_field() {
        // Fired, return 2 (bits 1-2 = 10), mode 3 (bits 3-4 = 11), no function, red-eye.
        let bits = 0b100_1101 | 0b0001_1000;
        let XmpValue::Structured(fields) = flash_to_xmp(&Value::Short(vec![bits])).unwrap() else {
            panic!("not a structure")
        };
        let get = |name: &str| {
            fields
                .iter()
                .find(|f| f.name == name)
                .and_then(XmpProperty::text)
        };
        assert_eq!(get("Fired"), Some("True"));
        assert_eq!(get("Return"), Some("2"));
        assert_eq!(get("Mode"), Some("3"));
        assert_eq!(get("Function"), Some("False"));
        assert_eq!(get("RedEyeMode"), Some("True"));
        // ...and back.
        let prop = XmpProperty::new(ExifNs.uri(), "Flash", XmpValue::Structured(fields));
        assert_eq!(flash_to_exif(&prop), Some(Value::Short(vec![bits])));
    }

    #[test]
    fn flash_back_needs_fired_and_reads_numbers_or_booleans() {
        let ns = ExifNs.uri();
        let field =
            |name: &str, text: &str| XmpProperty::new(ns, name, XmpValue::Simple(text.into()));
        let flash = |fields| XmpProperty::new(ns, "Flash", XmpValue::Structured(fields));
        assert_eq!(flash_to_exif(&flash(vec![field("Mode", "1")])), None);
        assert_eq!(
            flash_to_exif(&flash(vec![field("Fired", "true"), field("Function", "1")])),
            Some(Value::Short(vec![0b10_0001]))
        );
        assert_eq!(
            flash_to_exif(&flash(vec![
                field("Fired", "maybe"),
                field("RedEyeMode", "TRUE")
            ])),
            Some(Value::Short(vec![0b100_0000]))
        );
        assert_eq!(flash_to_exif(&field("Flash", "1")), None);
    }

    #[test]
    fn a_coordinate_maps_to_degrees_and_decimal_minutes_with_its_letter() {
        let coord = Value::Rational(vec![(40, 1), (26, 1), (4602, 100)]);
        assert_eq!(
            coord_to_xmp(&coord, &ascii("N")).as_deref(),
            Some("40,26.7670000N")
        );
        // Minutes past 60 carry into the degrees.
        let carry = Value::Rational(vec![(1, 1), (90, 1), (0, 1)]);
        assert_eq!(
            coord_to_xmp(&carry, &ascii("W")).as_deref(),
            Some("2,30.0000000W")
        );
        assert_eq!(
            coord_to_xmp(&Value::Rational(vec![(1, 0), (0, 1), (0, 1)]), &ascii("N")),
            None
        );
        assert_eq!(
            coord_to_xmp(&Value::Rational(vec![(1, 1)]), &ascii("N")),
            None
        );
        assert_eq!(coord_to_xmp(&coord, &ascii("")), None);
    }

    #[test]
    fn a_coordinate_reads_back_in_either_xmp_form() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "GPSLatitude", XmpValue::Simple(text.into()))
        };
        let decimal = coord_to_exif(&prop("40,26.767N"), T::GpsLatitude, T::GpsLatitudeRef);
        assert_eq!(
            decimal,
            [
                // 0.767 minutes is 46.02 seconds, reduced.
                (
                    T::GpsLatitude,
                    Value::Rational(vec![(40, 1), (26, 1), (2301, 50)])
                ),
                (T::GpsLatitudeRef, ascii("N")),
            ]
        );
        let sexagesimal = coord_to_exif(&prop("40,26,46S"), T::GpsLatitude, T::GpsLatitudeRef);
        assert_eq!(
            sexagesimal[0].1,
            Value::Rational(vec![(40, 1), (26, 1), (46, 1)])
        );
        assert_eq!(sexagesimal[1].1, ascii("S"));
        for bad in ["40,26.767X", "40N", "40,26,1,2N", "4x,26N", "-1,0N"] {
            assert_eq!(
                coord_to_exif(&prop(bad), T::GpsLatitude, T::GpsLatitudeRef),
                [],
                "{bad}"
            );
        }
    }

    #[test]
    fn float_to_rational_picks_the_largest_power_of_ten_that_fits() {
        assert_eq!(float_to_rational(0.5), Some((1, 2)));
        assert_eq!(float_to_rational(2_147.0), Some((2_147, 1)));
        assert_eq!(float_to_rational(2_148.5), Some((4_297, 2)));
        assert_eq!(float_to_rational(214_749.5), Some((429_499, 2)));
        assert_eq!(float_to_rational(21_474_837.0), Some((21_474_836, 1)));
        assert_eq!(float_to_rational(3e9), None);
        assert_eq!(float_to_rational(-1.0), None);
    }

    #[test]
    fn user_comment_decodes_each_character_code() {
        let le = ByteOrder::LittleEndian;
        let with = |code: &[u8], body: &[u8]| Value::Undefined([code, body].concat());
        assert_eq!(
            comment_to_xmp(&with(b"ASCII\0\0\0", b"hi\0pad"), le).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(&[0; 8], b"x\0"), le).as_deref(),
            Some("x")
        );
        assert_eq!(
            comment_to_xmp(&with(b"JIS\0\0\0\0\0", b"a\0b"), le).as_deref(),
            Some("a\0b")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"h\0i\0"), le).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\0h\0i"), ByteOrder::BigEndian).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xFE\xFF\0h"), le).as_deref(),
            Some("h")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xFF\xFEh\0"), ByteOrder::BigEndian).as_deref(),
            Some("h")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xEF\xBB\xBFok"), le).as_deref(),
            Some("ok")
        );
        assert_eq!(comment_to_xmp(&with(b"UNICODE\0", b"\x00\xD8"), le), None);
        assert_eq!(
            comment_to_xmp(&Value::Undefined(b"short".to_vec()), le),
            None
        );
        assert_eq!(comment_to_xmp(&ascii("ASCII\0\0\0text"), le), None);
    }

    #[test]
    fn user_comment_encodes_as_ucs2_in_the_stream_byte_order() {
        assert_eq!(
            comment_to_exif("hé", ByteOrder::LittleEndian),
            Value::Undefined(b"UNICODE\0h\0\xE9\0".to_vec())
        );
        assert_eq!(
            comment_to_exif("hé", ByteOrder::BigEndian),
            Value::Undefined(b"UNICODE\0\0h\0\xE9".to_vec())
        );
    }

    #[test]
    fn a_date_takes_its_subsec_digits_as_the_fraction() {
        let mut source = exif();
        source.set_tag(T::DateTimeOriginal, ascii("2020:01:02 03:04:05"));
        fn date(source: &Exif) -> Option<String> {
            let value = source.get_tag(T::DateTimeOriginal).unwrap();
            date_to_xmp(source, value, Some(T::SubSecTimeOriginal))
        }
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        source.set_tag(T::SubSecTimeOriginal, ascii("52"));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05.52"));
        source.set_tag(T::SubSecTimeOriginal, ascii("1234567890123"));
        assert_eq!(
            date(&source).as_deref(),
            Some("2020-01-02T03:04:05.123456789")
        );
        source.set_tag(T::SubSecTimeOriginal, ascii("   "));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        source.set_tag(T::SubSecTimeOriginal, Value::Short(vec![5]));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        assert_eq!(
            date_to_xmp(&source, &ascii("    :  :     :  :  "), None),
            None
        );
    }

    #[test]
    fn scan_ints_follows_sscanf() {
        assert_eq!(
            scan_ints::<3>(" 2020: -1:+2 trailing", b"::"),
            Some([2020, -1, 2])
        );
        assert_eq!(scan_ints::<2>("1 2", b" "), Some([1, 2]));
        assert_eq!(scan_ints::<2>("12", b" "), None);
        assert_eq!(scan_ints::<2>("1-2", b":"), None);
        assert_eq!(scan_ints::<1>("x", b""), None);
        assert_eq!(parse_long(" 42 "), Some(42));
        assert_eq!(parse_long("4 2"), None);
    }

    #[test]
    fn gps_time_combines_the_date_stamp_and_a_nine_place_fraction() {
        let mut source = exif();
        let stamp = Value::Rational(vec![(13, 1), (58, 1), (155, 10)]);
        assert_eq!(
            gps_time_to_xmp(&source, &stamp),
            None,
            "no date to put it on"
        );
        source.set_tag(T::DateTimeDigitized, ascii("2001:02:03 00:00:00"));
        assert_eq!(
            gps_time_to_xmp(&source, &stamp).as_deref(),
            Some("2001-02-03T13:58:15.500000000")
        );
        source.set_tag(T::GpsDateStamp, ascii("2008:07:19"));
        assert_eq!(
            gps_time_to_xmp(&source, &stamp).as_deref(),
            Some("2008-07-19T13:58:15.500000000")
        );
        assert_eq!(
            gps_time_to_xmp(&source, &Value::Rational(vec![(1, 0), (0, 1), (0, 1)])),
            None
        );
        assert_eq!(
            gps_time_to_xmp(&source, &Value::Rational(vec![(1, 1)])),
            None
        );
    }

    #[test]
    fn an_xmp_date_parses_every_precision_and_drops_its_zone() {
        let fields = |text: &str| parse_xmp_date(text).map(|d| (d.fields, d.fraction));
        assert_eq!(fields("2020"), Some(([2020, 0, 0, 0, 0, 0], String::new())));
        assert_eq!(
            fields("2020-05-06"),
            Some(([2020, 5, 6, 0, 0, 0], String::new()))
        );
        assert_eq!(
            fields("2020-05-06T07:08"),
            Some(([2020, 5, 6, 7, 8, 0], String::new()))
        );
        assert_eq!(
            fields("2020-05-06T07:08:09.0123456789+02:00"),
            Some(([2020, 5, 6, 7, 8, 9], "012345678".into()))
        );
        assert_eq!(
            fields("2020-05-06T07:08:09Z").unwrap().0,
            [2020, 5, 6, 7, 8, 9]
        );
        for bad in [
            "20",
            "2020-5",
            "2020-05-06T07",
            "2020-05-06T07:08:09.",
            "2020-05-06T07:08+2",
            "2020-05-06-07",
            "2020-05-06T07:08:09:10",
            "2020-05-06T07:08Q",
        ] {
            assert_eq!(parse_xmp_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_xmp_date_writes_the_exif_date_and_a_nine_digit_subsec() {
        let prop = |text: &str| {
            XmpProperty::new(
                Photoshop.uri(),
                "DateCreated",
                XmpValue::Simple(text.into()),
            )
        };
        let row = row_for(T::DateTimeOriginal);
        assert_eq!(
            to_exif(
                &prop("2020-05-06T07:08:09.05-05:00"),
                row,
                ByteOrder::LittleEndian
            ),
            [
                (T::DateTimeOriginal, ascii("2020:05:06 07:08:09")),
                (T::SubSecTimeOriginal, ascii("050000000")),
            ]
        );
        assert_eq!(
            to_exif(
                &prop("2020-05-06T07:08:09.000"),
                row,
                ByteOrder::LittleEndian
            ),
            [(T::DateTimeOriginal, ascii("2020:05:06 07:08:09"))]
        );
        assert_eq!(
            to_exif(&prop("not a date"), row, ByteOrder::LittleEndian),
            []
        );
    }

    #[test]
    fn an_xmp_date_writes_gps_time_and_date_stamps() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "GPSTimeStamp", XmpValue::Simple(text.into()))
        };
        let stamps = |text: &str| gps_time_to_exif(&prop(text));
        assert_eq!(
            stamps("2008-07-19T13:58:15Z"),
            [
                (
                    T::GpsTimeStamp,
                    Value::Rational(vec![(13, 1), (58, 1), (15, 1)])
                ),
                (T::GpsDateStamp, ascii("2008:07:19")),
            ]
        );
        assert_eq!(
            stamps("2008-07-19T13:58:15.5")[0].1,
            Value::Rational(vec![
                (13, 1),
                (58 * 60 + 15, 60),
                (500_000_000, 1_000_000_000)
            ])
        );
        assert_eq!(
            stamps("2008-07-19T13:58:00.5")[0].1,
            Value::Rational(vec![(13, 1), (58, 1), (500_000_000, 1_000_000_000)])
        );
        assert_eq!(stamps("garbage"), []);
    }

    #[test]
    fn xmp_text_reads_simple_alternative_and_array_values() {
        let p = |value| XmpProperty::new(Dc.uri(), "x", value);
        let alt = |items| p(XmpValue::Array(XmpArray::Alt(items)));
        assert_eq!(
            xmp_text(&p(XmpValue::Uri("u".into()))).as_deref(),
            Some("u")
        );
        assert_eq!(
            xmp_text(&alt(vec![
                XmpItem::lang_text("fr", "b"),
                XmpItem::lang_text("x-default", "a")
            ]))
            .as_deref(),
            Some("a")
        );
        assert_eq!(
            xmp_text(&alt(vec![XmpItem::lang_text("fr", "b")])).as_deref(),
            Some("b")
        );
        assert_eq!(
            xmp_text(&alt(vec![
                XmpItem::lang_text("fr", "b"),
                XmpItem::lang_text("de", "c")
            ])),
            None
        );
        let seq = p(XmpValue::Array(XmpArray::Seq(vec![
            XmpItem::simple("A"),
            XmpItem::simple("B"),
        ])));
        assert_eq!(xmp_text(&seq).as_deref(), Some("A, B"));
        assert_eq!(xmp_text(&p(XmpValue::Structured(Vec::new()))), None);
    }

    #[test]
    fn numeric_properties_read_one_value_per_token_in_the_tags_type() {
        let p = |value| XmpProperty::new(Tiff.uri(), "x", value);
        let seq = |items: &[&str]| {
            p(XmpValue::Array(XmpArray::Seq(
                items.iter().map(|&t| XmpItem::simple(t)).collect(),
            )))
        };
        let simple = |text: &str| p(XmpValue::Simple(text.into()));
        assert_eq!(
            plain_to_exif(&seq(&["8", "8", "8"]), T::BitsPerSample),
            Some(Value::Short(vec![8, 8, 8]))
        );
        assert_eq!(
            plain_to_exif(&seq(&["8 8 8"]), T::BitsPerSample),
            Some(Value::Short(vec![8, 8, 8]))
        );
        assert_eq!(
            plain_to_exif(&simple("70000"), T::ImageWidth),
            Some(Value::Long(vec![70_000]))
        );
        assert_eq!(
            plain_to_exif(&simple("640"), T::ImageWidth),
            Some(Value::Short(vec![640]))
        );
        assert_eq!(plain_to_exif(&simple("70000"), T::Orientation), None);
        assert_eq!(
            plain_to_exif(&simple("3"), T::Rating),
            Some(Value::Short(vec![3]))
        );
        assert_eq!(
            plain_to_exif(&simple("72/1"), T::XResolution),
            Some(Value::Rational(vec![(72, 1)]))
        );
        assert_eq!(plain_to_exif(&simple("72"), T::XResolution), None);
        assert_eq!(
            plain_to_exif(&simple("-1/3"), T::ExposureBiasValue),
            Some(Value::SRational(vec![(-1, 3)]))
        );
        assert_eq!(plain_to_exif(&simple("-1"), T::ExposureBiasValue), None);
        assert_eq!(
            plain_to_exif(&simple("3"), T::FileSource),
            Some(Value::Undefined(vec![3]))
        );
        assert_eq!(
            plain_to_exif(&simple("1"), T::GpsAltitudeRef),
            Some(Value::Byte(vec![1]))
        );
        assert_eq!(plain_to_exif(&simple("256"), T::GpsAltitudeRef), None);
        assert_eq!(plain_to_exif(&simple(""), T::Orientation), None);
    }

    #[test]
    fn text_properties_write_ascii_or_utf8_where_the_tag_allows_it() {
        let p = |text: &str| XmpProperty::new(Tiff.uri(), "x", XmpValue::Simple(text.into()));
        assert_eq!(plain_to_exif(&p("Canon"), T::Make), Some(ascii("Canon")));
        assert_eq!(
            plain_to_exif(&p("Café"), T::Make),
            Some(Value::Utf8("Café".into()))
        );
        assert_eq!(
            plain_to_exif(&p("Café"), T::GpsMapDatum),
            Some(ascii("Café"))
        );
    }

    #[test]
    fn versions_read_back_from_their_first_four_characters() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "ExifVersion", XmpValue::Simple(text.into()))
        };
        assert_eq!(
            version_to_exif(&prop("02321")),
            Some(Value::Undefined(b"0232".to_vec()))
        );
        assert_eq!(version_to_exif(&prop("023")), None);
        let gps = row_for(T::GpsVersionId);
        let gps_prop = XmpProperty::new(
            ExifNs.uri(),
            "GPSVersionID",
            XmpValue::Simple("2.2.0.0".into()),
        );
        assert_eq!(
            to_exif(&gps_prop, gps, ByteOrder::LittleEndian),
            [(T::GpsVersionId, Value::Byte(vec![2, 2, 0, 0]))]
        );
    }

    #[test]
    fn conversion_skips_absent_and_unconvertible_sources() {
        let mut source = exif();
        source.set_tag(T::Orientation, Value::Float(vec![1.0]));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert!(xmp.properties.is_empty());

        xmp.set_text(Tiff.uri(), "Orientation", "sideways");
        let mut target = exif();
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.orientation(), None);
    }

    #[test]
    fn a_gps_coordinate_round_trips_with_its_reference() {
        let mut source = exif();
        source.set_tag(
            T::GpsLatitude,
            Value::Rational(vec![(40, 1), (26, 1), (46, 1)]),
        );
        source.set_tag(T::GpsLatitudeRef, ascii("N"));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::default());
        assert_eq!(
            xmp.get_text(ExifNs.uri(), "GPSLatitude"),
            Some("40,26.7666667N")
        );
        let mut target = exif();
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::default());
        assert_eq!(target.get_tag(T::GpsLatitudeRef), Some(&ascii("N")));
        assert!(target.get_tag(T::GpsLatitude).is_some());
    }
}
