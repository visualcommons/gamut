//! The EXIF ↔ XMP mapping table: one [`Row`] per mapped tag, in exiv2's `conversion_` order.

use gamut_exif::ExifTag;
use gamut_xmp::WellKnownNs;

/// The XMP shape a plain row's value takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shape {
    /// A simple text value.
    Text,
    /// An ordered array: one entry per numeric value, or one entry holding a text value.
    Seq,
    /// A language alternative with one `x-default` entry.
    LangAlt,
}

/// How a row converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Conv {
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
pub(super) struct Row {
    pub(super) tag: ExifTag,
    pub(super) ns: WellKnownNs,
    pub(super) name: &'static str,
    pub(super) conv: Conv,
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
pub(super) const ROWS: &[Row] = &[
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_maps_a_distinct_tag_and_a_distinct_property() {
        for (i, a) in ROWS.iter().enumerate() {
            for b in &ROWS[i + 1..] {
                assert_ne!(a.tag, b.tag);
                assert_ne!((a.ns, a.name), (b.ns, b.name));
            }
        }
    }
}
