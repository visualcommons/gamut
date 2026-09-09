//! The EXIF tag dictionary and the directories tags live in.
//!
//! [`IfdKind`] classifies which directory a tag belongs to; [`ExifTag`] names the standard Exif 3.0
//! tags. The catalogue is generated from a single table (the [`exif_tags!`] invocation), so a tag's
//! id, home directory, canonical name, permitted field types and component count can never drift
//! apart. It intentionally covers the **standard** CIPA DC-008 tags, not exiftool's full vendor
//! breadth — unknown and MakerNote tags still round-trip losslessly because [`Exif`](crate::Exif)
//! retains the raw [`gamut_ifd::Ifd`].
//!
//! # What the spec constrains
//!
//! Every tag CIPA DC-008 itself defines carries the `Type` and `Count` columns of the table that
//! defines it — Table 6 (0th IFD), Tables 8 and 9 (Exif sub-IFD), Table 14 (GPS) and Table 16
//! (Interoperability) — as [`ExifTag::field_types`] and [`ExifTag::component_count`]. A handful of
//! catalogued tags come from **other** specifications and are carried only for compatibility
//! (`ApplicationNotes`, `IPTC-NAA`, `InterColorProfile`, `Rating`, `RatingPercent`, and the DCF-era
//! Interoperability tags beyond `InteroperabilityIndex`); DC-008 mandates nothing for them, so
//! their `field_types` list is **empty** and no constraint is claimed.
//!
//! These two columns describe what a *conformant writer* must emit. They are used by
//! [`set_tag_checked`](crate::set_tag_checked) on the write path only: the reader stays lenient and
//! accepts whatever a real-world file carries.

use gamut_ifd::FieldType;

use crate::tag::TagCount::{Any, Exact, OneOf};

/// Which IFD a tag belongs to.
///
/// The same 16-bit tag number can mean different things in different directories (e.g. `0x0001` is
/// `GPSLatitudeRef` in [`IfdKind::Gps`] but `InteroperabilityIndex` in [`IfdKind::Interop`]), so a
/// tag is only fully identified by the pair (`IfdKind`, id).
///
/// The 1st IFD ([`IfdKind::Thumbnail`]) reuses the 0th IFD's TIFF baseline tags rather than
/// defining its own, so no [`ExifTag`] is classified under `Thumbnail`; [`ExifTag::from_id`]
/// resolves a `Thumbnail` lookup against the [`IfdKind::Image`] tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IfdKind {
    /// The 0th IFD — primary-image / TIFF tags (Make, Model, Orientation, resolution, …).
    Image,
    /// The Exif sub-IFD — capture parameters (exposure, aperture, ISO, lens, …).
    Exif,
    /// The GPS sub-IFD — positioning data.
    Gps,
    /// The Interoperability sub-IFD — interoperability identification.
    Interop,
    /// The 1st IFD — the embedded thumbnail's tags (shares the 0th IFD's tag definitions).
    Thumbnail,
}

/// How many components CIPA DC-008 requires a tag's value to have — the `Count` column of the
/// table that defines the tag.
///
/// For the string types (`ASCII` and the Exif 3.0 `UTF8`) a count is a **byte** count *including*
/// the terminating NUL, matching [`gamut_ifd::Value::count`]: `DateTime`'s count of 20 is the 19
/// characters of `YYYY:MM:DD HH:MM:SS` plus the NUL.
///
/// Ask [`TagCount::allows`] rather than matching the variants — it is the whole of the behaviour
/// and stays correct as variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TagCount {
    /// Exactly this many components.
    Exact(u32),
    /// Any component count: the spec's `Any`, and the image-geometry-dependent `*S` strip counts
    /// of `StripOffsets`/`StripByteCounts` (Table 6), which no fixed number can express.
    Any,
    /// One of a fixed set of counts, in ascending order — `SubjectArea`'s "2 or 3 or 4".
    OneOf(&'static [u32]),
}

impl TagCount {
    /// Whether `count` components satisfy this requirement.
    #[must_use]
    pub fn allows(self, count: u64) -> bool {
        match self {
            TagCount::Any => true,
            TagCount::Exact(n) => count == u64::from(n),
            TagCount::OneOf(ns) => ns.iter().any(|&n| count == u64::from(n)),
        }
    }
}

/// Generates the [`ExifTag`] enum and its accessors from one table of
/// `Variant => (IfdKind, id, "CanonicalName", [FieldType, …], TagCount)` rows, keeping them in
/// lock-step.
macro_rules! exif_tags {
    ($($variant:ident => ($ifd:ident, $id:expr, $name:expr, [$($ty:ident),*], $count:expr)),+ $(,)?) => {
        /// A standard EXIF tag.
        ///
        /// `#[non_exhaustive]` so tags can be added post-1.0 without a breaking change. Each variant
        /// maps to a 16-bit on-disk tag number ([`ExifTag::tag_id`]) within its home directory
        /// ([`ExifTag::ifd`]); [`ExifTag::name`] gives the canonical CIPA DC-008 name, and
        /// [`ExifTag::field_types`]/[`ExifTag::component_count`] the value shape the spec mandates.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum ExifTag {
            $(#[doc = $name] $variant,)+
        }

        impl ExifTag {
            /// Every catalogued tag, in declaration order.
            pub const ALL: &'static [ExifTag] = &[$(ExifTag::$variant),+];

            /// The 16-bit on-disk tag number.
            #[must_use]
            pub const fn tag_id(self) -> u16 {
                match self { $(ExifTag::$variant => $id),+ }
            }

            /// The directory this tag belongs to.
            #[must_use]
            pub const fn ifd(self) -> IfdKind {
                match self { $(ExifTag::$variant => IfdKind::$ifd),+ }
            }

            /// The canonical CIPA DC-008 tag name (stable across variant renames).
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $(ExifTag::$variant => $name),+ }
            }

            /// The field types CIPA DC-008 permits for this tag, in the order the spec lists them
            /// (`SHORT or LONG` reads as `[Short, Long]`).
            ///
            /// **Empty** for a catalogued tag CIPA DC-008 does not define — see the [module
            /// docs](self). An empty list is not "no valid type"; it is "this specification claims
            /// nothing", and [`set_tag_checked`](crate::set_tag_checked) accepts any value there.
            #[must_use]
            pub const fn field_types(self) -> &'static [FieldType] {
                match self { $(ExifTag::$variant => &[$(FieldType::$ty),*]),+ }
            }

            /// The component count CIPA DC-008 requires for this tag.
            ///
            /// [`TagCount::Any`] for a tag the spec leaves open and for one it does not define.
            #[must_use]
            pub const fn component_count(self) -> TagCount {
                match self { $(ExifTag::$variant => $count),+ }
            }
        }
    };
}

impl ExifTag {
    /// Looks up a tag by its directory and on-disk id, or `None` if it is not a catalogued standard
    /// tag.
    ///
    /// A [`IfdKind::Thumbnail`] lookup resolves against the [`IfdKind::Image`] tags, since the 1st
    /// IFD reuses the 0th IFD's TIFF baseline definitions.
    #[must_use]
    pub fn from_id(ifd: IfdKind, id: u16) -> Option<ExifTag> {
        let ifd = match ifd {
            IfdKind::Thumbnail => IfdKind::Image,
            other => other,
        };
        ExifTag::ALL
            .iter()
            .copied()
            .find(|t| t.tag_id() == id && t.ifd() == ifd)
    }
}

exif_tags! {
    // ---- 0th IFD (TIFF baseline + EXIF-added image attributes); also used by the 1st IFD ----
    // CIPA DC-008 Table 6 "TIFF Rev. 6.0 Attribute Information Used in Exif" (§4.6.5).
    ImageWidth => (Image, 0x0100, "ImageWidth", [Short, Long], Exact(1)),
    ImageLength => (Image, 0x0101, "ImageLength", [Short, Long], Exact(1)),
    BitsPerSample => (Image, 0x0102, "BitsPerSample", [Short], Exact(3)),
    Compression => (Image, 0x0103, "Compression", [Short], Exact(1)),
    PhotometricInterpretation => (Image, 0x0106, "PhotometricInterpretation", [Short], Exact(1)),
    ImageDescription => (Image, 0x010E, "ImageDescription", [Ascii, Utf8], Any),
    Make => (Image, 0x010F, "Make", [Ascii, Utf8], Any),
    Model => (Image, 0x0110, "Model", [Ascii, Utf8], Any),
    // Count `*S` in Table 6: StripsPerImage, which depends on the image geometry.
    StripOffsets => (Image, 0x0111, "StripOffsets", [Short, Long], Any),
    Orientation => (Image, 0x0112, "Orientation", [Short], Exact(1)),
    SamplesPerPixel => (Image, 0x0115, "SamplesPerPixel", [Short], Exact(1)),
    RowsPerStrip => (Image, 0x0116, "RowsPerStrip", [Short, Long], Exact(1)),
    StripByteCounts => (Image, 0x0117, "StripByteCounts", [Short, Long], Any),
    XResolution => (Image, 0x011A, "XResolution", [Rational], Exact(1)),
    YResolution => (Image, 0x011B, "YResolution", [Rational], Exact(1)),
    PlanarConfiguration => (Image, 0x011C, "PlanarConfiguration", [Short], Exact(1)),
    ResolutionUnit => (Image, 0x0128, "ResolutionUnit", [Short], Exact(1)),
    // Table 6 gives the count as `3 * 256`: one 256-entry curve per colour component.
    TransferFunction => (Image, 0x012D, "TransferFunction", [Short], Exact(768)),
    Software => (Image, 0x0131, "Software", [Ascii, Utf8], Any),
    DateTime => (Image, 0x0132, "DateTime", [Ascii], Exact(20)),
    Artist => (Image, 0x013B, "Artist", [Ascii, Utf8], Any),
    WhitePoint => (Image, 0x013E, "WhitePoint", [Rational], Exact(2)),
    PrimaryChromaticities => (Image, 0x013F, "PrimaryChromaticities", [Rational], Exact(6)),
    JpegInterchangeFormat => (Image, 0x0201, "JPEGInterchangeFormat", [Long], Exact(1)),
    JpegInterchangeFormatLength => (Image, 0x0202, "JPEGInterchangeFormatLength", [Long], Exact(1)),
    YCbCrCoefficients => (Image, 0x0211, "YCbCrCoefficients", [Rational], Exact(3)),
    YCbCrSubSampling => (Image, 0x0212, "YCbCrSubSampling", [Short], Exact(2)),
    YCbCrPositioning => (Image, 0x0213, "YCbCrPositioning", [Short], Exact(1)),
    ReferenceBlackWhite => (Image, 0x0214, "ReferenceBlackWhite", [Rational], Exact(6)),
    Copyright => (Image, 0x8298, "Copyright", [Ascii, Utf8], Any),
    // Carried for compatibility and defined by specifications other than CIPA DC-008, so no field
    // type or count is claimed and nothing is validated (see the module docs).
    Xmp => (Image, 0x02BC, "ApplicationNotes", [], Any),
    IptcNaa => (Image, 0x83BB, "IPTC-NAA", [], Any),
    InterColorProfile => (Image, 0x8773, "InterColorProfile", [], Any),
    Rating => (Image, 0x4746, "Rating", [], Any),
    RatingPercent => (Image, 0x4749, "RatingPercent", [], Any),

    // ---- Exif sub-IFD (capture parameters) ----
    // CIPA DC-008 Table 8 / Table 9 "Exif IFD Attribute Information" (§4.6.6).
    ExposureTime => (Exif, 0x829A, "ExposureTime", [Rational], Exact(1)),
    FNumber => (Exif, 0x829D, "FNumber", [Rational], Exact(1)),
    ExposureProgram => (Exif, 0x8822, "ExposureProgram", [Short], Exact(1)),
    SpectralSensitivity => (Exif, 0x8824, "SpectralSensitivity", [Ascii], Any),
    PhotographicSensitivity => (Exif, 0x8827, "PhotographicSensitivity", [Short], Any),
    Oecf => (Exif, 0x8828, "OECF", [Undefined], Any),
    SensitivityType => (Exif, 0x8830, "SensitivityType", [Short], Exact(1)),
    StandardOutputSensitivity => (Exif, 0x8831, "StandardOutputSensitivity", [Long], Exact(1)),
    RecommendedExposureIndex => (Exif, 0x8832, "RecommendedExposureIndex", [Long], Exact(1)),
    IsoSpeed => (Exif, 0x8833, "ISOSpeed", [Long], Exact(1)),
    IsoSpeedLatitudeYyy => (Exif, 0x8834, "ISOSpeedLatitudeyyy", [Long], Exact(1)),
    IsoSpeedLatitudeZzz => (Exif, 0x8835, "ISOSpeedLatitudezzz", [Long], Exact(1)),
    ExifVersion => (Exif, 0x9000, "ExifVersion", [Undefined], Exact(4)),
    DateTimeOriginal => (Exif, 0x9003, "DateTimeOriginal", [Ascii], Exact(20)),
    DateTimeDigitized => (Exif, 0x9004, "DateTimeDigitized", [Ascii], Exact(20)),
    OffsetTime => (Exif, 0x9010, "OffsetTime", [Ascii], Exact(7)),
    OffsetTimeOriginal => (Exif, 0x9011, "OffsetTimeOriginal", [Ascii], Exact(7)),
    OffsetTimeDigitized => (Exif, 0x9012, "OffsetTimeDigitized", [Ascii], Exact(7)),
    ComponentsConfiguration => (Exif, 0x9101, "ComponentsConfiguration", [Undefined], Exact(4)),
    CompressedBitsPerPixel => (Exif, 0x9102, "CompressedBitsPerPixel", [Rational], Exact(1)),
    ShutterSpeedValue => (Exif, 0x9201, "ShutterSpeedValue", [SRational], Exact(1)),
    ApertureValue => (Exif, 0x9202, "ApertureValue", [Rational], Exact(1)),
    BrightnessValue => (Exif, 0x9203, "BrightnessValue", [SRational], Exact(1)),
    ExposureBiasValue => (Exif, 0x9204, "ExposureBiasValue", [SRational], Exact(1)),
    MaxApertureValue => (Exif, 0x9205, "MaxApertureValue", [Rational], Exact(1)),
    SubjectDistance => (Exif, 0x9206, "SubjectDistance", [Rational], Exact(1)),
    MeteringMode => (Exif, 0x9207, "MeteringMode", [Short], Exact(1)),
    LightSource => (Exif, 0x9208, "LightSource", [Short], Exact(1)),
    Flash => (Exif, 0x9209, "Flash", [Short], Exact(1)),
    FocalLength => (Exif, 0x920A, "FocalLength", [Rational], Exact(1)),
    // §4.6.6.7.22: a point (2), a circle (3), or a rectangle (4).
    SubjectArea => (Exif, 0x9214, "SubjectArea", [Short], OneOf(&[2, 3, 4])),
    MakerNote => (Exif, 0x927C, "MakerNote", [Undefined], Any),
    UserComment => (Exif, 0x9286, "UserComment", [Undefined], Any),
    SubSecTime => (Exif, 0x9290, "SubSecTime", [Ascii], Any),
    SubSecTimeOriginal => (Exif, 0x9291, "SubSecTimeOriginal", [Ascii], Any),
    SubSecTimeDigitized => (Exif, 0x9292, "SubSecTimeDigitized", [Ascii], Any),
    Temperature => (Exif, 0x9400, "Temperature", [SRational], Exact(1)),
    Humidity => (Exif, 0x9401, "Humidity", [Rational], Exact(1)),
    Pressure => (Exif, 0x9402, "Pressure", [Rational], Exact(1)),
    WaterDepth => (Exif, 0x9403, "WaterDepth", [SRational], Exact(1)),
    Acceleration => (Exif, 0x9404, "Acceleration", [Rational], Exact(1)),
    CameraElevationAngle => (Exif, 0x9405, "CameraElevationAngle", [SRational], Exact(1)),
    FlashpixVersion => (Exif, 0xA000, "FlashpixVersion", [Undefined], Exact(4)),
    ColorSpace => (Exif, 0xA001, "ColorSpace", [Short], Exact(1)),
    PixelXDimension => (Exif, 0xA002, "PixelXDimension", [Short, Long], Exact(1)),
    PixelYDimension => (Exif, 0xA003, "PixelYDimension", [Short, Long], Exact(1)),
    RelatedSoundFile => (Exif, 0xA004, "RelatedSoundFile", [Ascii], Exact(13)),
    FlashEnergy => (Exif, 0xA20B, "FlashEnergy", [Rational], Exact(1)),
    SpatialFrequencyResponse => (Exif, 0xA20C, "SpatialFrequencyResponse", [Undefined], Any),
    FocalPlaneXResolution => (Exif, 0xA20E, "FocalPlaneXResolution", [Rational], Exact(1)),
    FocalPlaneYResolution => (Exif, 0xA20F, "FocalPlaneYResolution", [Rational], Exact(1)),
    FocalPlaneResolutionUnit => (Exif, 0xA210, "FocalPlaneResolutionUnit", [Short], Exact(1)),
    SubjectLocation => (Exif, 0xA214, "SubjectLocation", [Short], Exact(2)),
    ExposureIndex => (Exif, 0xA215, "ExposureIndex", [Rational], Exact(1)),
    SensingMethod => (Exif, 0xA217, "SensingMethod", [Short], Exact(1)),
    FileSource => (Exif, 0xA300, "FileSource", [Undefined], Exact(1)),
    SceneType => (Exif, 0xA301, "SceneType", [Undefined], Exact(1)),
    CfaPattern => (Exif, 0xA302, "CFAPattern", [Undefined], Any),
    CustomRendered => (Exif, 0xA401, "CustomRendered", [Short], Exact(1)),
    ExposureMode => (Exif, 0xA402, "ExposureMode", [Short], Exact(1)),
    WhiteBalance => (Exif, 0xA403, "WhiteBalance", [Short], Exact(1)),
    DigitalZoomRatio => (Exif, 0xA404, "DigitalZoomRatio", [Rational], Exact(1)),
    FocalLengthIn35mmFilm => (Exif, 0xA405, "FocalLengthIn35mmFilm", [Short], Exact(1)),
    SceneCaptureType => (Exif, 0xA406, "SceneCaptureType", [Short], Exact(1)),
    GainControl => (Exif, 0xA407, "GainControl", [Rational], Exact(1)),
    Contrast => (Exif, 0xA408, "Contrast", [Short], Exact(1)),
    Saturation => (Exif, 0xA409, "Saturation", [Short], Exact(1)),
    Sharpness => (Exif, 0xA40A, "Sharpness", [Short], Exact(1)),
    DeviceSettingDescription => (Exif, 0xA40B, "DeviceSettingDescription", [Undefined], Any),
    SubjectDistanceRange => (Exif, 0xA40C, "SubjectDistanceRange", [Short], Exact(1)),
    ImageUniqueId => (Exif, 0xA420, "ImageUniqueID", [Ascii], Exact(33)),
    CameraOwnerName => (Exif, 0xA430, "CameraOwnerName", [Ascii, Utf8], Any),
    BodySerialNumber => (Exif, 0xA431, "BodySerialNumber", [Ascii], Any),
    LensSpecification => (Exif, 0xA432, "LensSpecification", [Rational], Exact(4)),
    LensMake => (Exif, 0xA433, "LensMake", [Ascii, Utf8], Any),
    LensModel => (Exif, 0xA434, "LensModel", [Ascii, Utf8], Any),
    LensSerialNumber => (Exif, 0xA435, "LensSerialNumber", [Ascii], Any),
    CompositeImage => (Exif, 0xA460, "CompositeImage", [Short], Exact(1)),
    SourceImageNumberOfCompositeImage => (Exif, 0xA461, "SourceImageNumberOfCompositeImage", [Short], Exact(2)),
    SourceExposureTimesOfCompositeImage => (Exif, 0xA462, "SourceExposureTimesOfCompositeImage", [Undefined], Any),
    Gamma => (Exif, 0xA500, "Gamma", [Rational], Exact(1)),

    // ---- GPS sub-IFD ----
    // CIPA DC-008 Table 14 "GPS Attribute Information" (§4.6.7).
    GpsVersionId => (Gps, 0x0000, "GPSVersionID", [Byte], Exact(4)),
    GpsLatitudeRef => (Gps, 0x0001, "GPSLatitudeRef", [Ascii], Exact(2)),
    GpsLatitude => (Gps, 0x0002, "GPSLatitude", [Rational], Exact(3)),
    GpsLongitudeRef => (Gps, 0x0003, "GPSLongitudeRef", [Ascii], Exact(2)),
    GpsLongitude => (Gps, 0x0004, "GPSLongitude", [Rational], Exact(3)),
    GpsAltitudeRef => (Gps, 0x0005, "GPSAltitudeRef", [Byte], Exact(1)),
    GpsAltitude => (Gps, 0x0006, "GPSAltitude", [Rational], Exact(1)),
    GpsTimeStamp => (Gps, 0x0007, "GPSTimeStamp", [Rational], Exact(3)),
    GpsSatellites => (Gps, 0x0008, "GPSSatellites", [Ascii], Any),
    GpsStatus => (Gps, 0x0009, "GPSStatus", [Ascii], Exact(2)),
    GpsMeasureMode => (Gps, 0x000A, "GPSMeasureMode", [Ascii], Exact(2)),
    GpsDop => (Gps, 0x000B, "GPSDOP", [Rational], Exact(1)),
    GpsSpeedRef => (Gps, 0x000C, "GPSSpeedRef", [Ascii], Exact(2)),
    GpsSpeed => (Gps, 0x000D, "GPSSpeed", [Rational], Exact(1)),
    GpsTrackRef => (Gps, 0x000E, "GPSTrackRef", [Ascii], Exact(2)),
    GpsTrack => (Gps, 0x000F, "GPSTrack", [Rational], Exact(1)),
    GpsImgDirectionRef => (Gps, 0x0010, "GPSImgDirectionRef", [Ascii], Exact(2)),
    GpsImgDirection => (Gps, 0x0011, "GPSImgDirection", [Rational], Exact(1)),
    GpsMapDatum => (Gps, 0x0012, "GPSMapDatum", [Ascii], Any),
    GpsDestLatitudeRef => (Gps, 0x0013, "GPSDestLatitudeRef", [Ascii], Exact(2)),
    GpsDestLatitude => (Gps, 0x0014, "GPSDestLatitude", [Rational], Exact(3)),
    GpsDestLongitudeRef => (Gps, 0x0015, "GPSDestLongitudeRef", [Ascii], Exact(2)),
    GpsDestLongitude => (Gps, 0x0016, "GPSDestLongitude", [Rational], Exact(3)),
    GpsDestBearingRef => (Gps, 0x0017, "GPSDestBearingRef", [Ascii], Exact(2)),
    GpsDestBearing => (Gps, 0x0018, "GPSDestBearing", [Rational], Exact(1)),
    GpsDestDistanceRef => (Gps, 0x0019, "GPSDestDistanceRef", [Ascii], Exact(2)),
    GpsDestDistance => (Gps, 0x001A, "GPSDestDistance", [Rational], Exact(1)),
    GpsProcessingMethod => (Gps, 0x001B, "GPSProcessingMethod", [Undefined], Any),
    GpsAreaInformation => (Gps, 0x001C, "GPSAreaInformation", [Undefined], Any),
    GpsDateStamp => (Gps, 0x001D, "GPSDateStamp", [Ascii], Exact(11)),
    GpsDifferential => (Gps, 0x001E, "GPSDifferential", [Short], Exact(1)),
    GpsHPositioningError => (Gps, 0x001F, "GPSHPositioningError", [Rational], Exact(1)),

    // ---- Interoperability sub-IFD ----
    // CIPA DC-008 Table 16 defines only InteroperabilityIndex; the rest are DCF-era tags carried
    // for compatibility, so no field type or count is claimed for them.
    InteroperabilityIndex => (Interop, 0x0001, "InteroperabilityIndex", [Ascii], Any),
    InteroperabilityVersion => (Interop, 0x0002, "InteroperabilityVersion", [], Any),
    RelatedImageFileFormat => (Interop, 0x1000, "RelatedImageFileFormat", [], Any),
    RelatedImageWidth => (Interop, 0x1001, "RelatedImageWidth", [], Any),
    RelatedImageLength => (Interop, 0x1002, "RelatedImageLength", [], Any),
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn tag_ids_are_unique_within_each_ifd() {
        // Two tags sharing an (ifd, id) would make the catalogue ambiguous and break from_id.
        let mut seen = HashSet::new();
        for &tag in ExifTag::ALL {
            assert!(
                seen.insert((tag.ifd(), tag.tag_id())),
                "duplicate (ifd, id) for {tag:?}"
            );
        }
    }

    #[test]
    fn from_id_round_trips_every_tag() {
        for &tag in ExifTag::ALL {
            assert_eq!(ExifTag::from_id(tag.ifd(), tag.tag_id()), Some(tag));
        }
    }

    #[test]
    fn same_id_disambiguated_by_ifd() {
        // 0x0001 is GPSLatitudeRef in GPS but InteroperabilityIndex in Interop.
        assert_eq!(
            ExifTag::from_id(IfdKind::Gps, 0x0001),
            Some(ExifTag::GpsLatitudeRef)
        );
        assert_eq!(
            ExifTag::from_id(IfdKind::Interop, 0x0001),
            Some(ExifTag::InteroperabilityIndex)
        );
        assert_eq!(ExifTag::GpsLatitudeRef.tag_id(), 0x0001);
        assert_eq!(ExifTag::InteroperabilityIndex.tag_id(), 0x0001);
    }

    #[test]
    fn thumbnail_resolves_against_image_baseline() {
        // The 1st IFD reuses the 0th IFD's tag definitions.
        assert_eq!(
            ExifTag::from_id(IfdKind::Thumbnail, 0x0103),
            Some(ExifTag::Compression)
        );
        assert_eq!(
            ExifTag::from_id(IfdKind::Thumbnail, 0x0201),
            Some(ExifTag::JpegInterchangeFormat)
        );
    }

    #[test]
    fn unknown_ids_are_none() {
        assert_eq!(ExifTag::from_id(IfdKind::Image, 0xFFFF), None);
        // A GPS id looked up in the Exif IFD does not resolve.
        assert_eq!(ExifTag::from_id(IfdKind::Exif, 0x0002), None);
    }

    #[test]
    fn name_and_ids_pin_representative_tags() {
        assert_eq!(ExifTag::FNumber.tag_id(), 0x829D);
        assert_eq!(ExifTag::FNumber.ifd(), IfdKind::Exif);
        assert_eq!(ExifTag::FNumber.name(), "FNumber");
        assert_eq!(ExifTag::Make.name(), "Make");
        assert_eq!(ExifTag::IptcNaa.name(), "IPTC-NAA");
        assert_eq!(ExifTag::GpsLatitude.tag_id(), 0x0002);
        assert!(ExifTag::ALL.len() > 140);
    }

    #[test]
    fn field_types_and_counts_pin_one_tag_of_each_shape() {
        // A single type with a fixed count.
        assert_eq!(ExifTag::FNumber.field_types(), &[FieldType::Rational]);
        assert_eq!(ExifTag::FNumber.component_count(), TagCount::Exact(1));
        // "SHORT or LONG" keeps the spec's order.
        assert_eq!(
            ExifTag::ImageWidth.field_types(),
            &[FieldType::Short, FieldType::Long]
        );
        // "ASCII or UTF-8" with an open count.
        assert_eq!(
            ExifTag::Make.field_types(),
            &[FieldType::Ascii, FieldType::Utf8]
        );
        assert_eq!(ExifTag::Make.component_count(), TagCount::Any);
        // A string count includes the NUL: 19 characters of DateTime plus one.
        assert_eq!(ExifTag::DateTime.component_count(), TagCount::Exact(20));
        // The one alternating count in the spec.
        assert_eq!(
            ExifTag::SubjectArea.component_count(),
            TagCount::OneOf(&[2, 3, 4])
        );
        // 3 * 256, written out.
        assert_eq!(
            ExifTag::TransferFunction.component_count(),
            TagCount::Exact(768)
        );
    }

    #[test]
    fn only_the_non_dc008_tags_claim_no_field_type() {
        // The boundary of what `references/exif` actually specifies: everything else in the
        // catalogue carries the Type column of the DC-008 table that defines it.
        let unspecified: Vec<&str> = ExifTag::ALL
            .iter()
            .filter(|t| t.field_types().is_empty())
            .map(|t| t.name())
            .collect();
        assert_eq!(
            unspecified,
            [
                "ApplicationNotes",
                "IPTC-NAA",
                "InterColorProfile",
                "Rating",
                "RatingPercent",
                "InteroperabilityVersion",
                "RelatedImageFileFormat",
                "RelatedImageWidth",
                "RelatedImageLength",
            ]
        );
    }

    #[test]
    fn a_specified_tag_never_lists_a_type_twice() {
        // A duplicated type would be a transcription slip that no other assertion would notice.
        for &tag in ExifTag::ALL {
            let types = tag.field_types();
            let unique: HashSet<u16> = types.iter().map(|t| t.code()).collect();
            assert_eq!(unique.len(), types.len(), "{} repeats a type", tag.name());
        }
    }

    #[test]
    fn count_allows_matches_the_requirement() {
        assert!(TagCount::Any.allows(0));
        assert!(TagCount::Any.allows(u64::MAX));

        assert!(TagCount::Exact(3).allows(3));
        assert!(!TagCount::Exact(3).allows(2));
        assert!(!TagCount::Exact(3).allows(4));

        let one_of = TagCount::OneOf(&[2, 3, 4]);
        assert!(!one_of.allows(1));
        assert!(one_of.allows(2));
        assert!(one_of.allows(4));
        assert!(!one_of.allows(5));
        // An empty set admits nothing, so `allows` cannot be short-circuiting to true.
        assert!(!TagCount::OneOf(&[]).allows(0));
    }

    #[test]
    fn one_of_counts_are_ascending_and_distinct() {
        // `TagCount::OneOf` documents ascending order; a caller may rely on it.
        for &tag in ExifTag::ALL {
            if let TagCount::OneOf(ns) = tag.component_count() {
                assert!(
                    ns.windows(2).all(|w| w[0] < w[1]),
                    "{} lists counts out of order",
                    tag.name()
                );
            }
        }
    }
}
