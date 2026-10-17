//! Well-known **structural** pointer tags, and the shared **tag registry**.
//!
//! This crate deliberately owns no tag *semantics* — what `ImageWidth` or `Compression` mean is
//! codec business. The pointer tags are the one principled exception, because they are
//! *structure*: their values locate other directories (or, for [`MAKER_NOTE`], a byte blob
//! vendors fill with a rebased mini-IFD), i.e. they name the directory graph itself — the graph
//! [`read_tree`](crate::read_tree) / [`IfdReader::read_tree`](crate::IfdReader::read_tree)
//! reconstruct and [`write`](crate::write) flattens. Before this module, every consumer
//! (gamut-tiff, gamut-dng, gamut-exif) duplicated them.
//!
//! # The tag registry
//!
//! A tag's *identity and value shape* — its number, the name its specification gives it, the
//! field types that specification permits, and the component count it requires — is not
//! semantics either, and it is shared: TIFF 6.0's tags are carried by every TIFF-based format, and
//! DNG's by any reader of a DNG's 0th IFD (gamut-tiff, gamut-dng and gamut-exif all are). The
//! registry states each once, as a [`TagSpec`] row, grouped into one [`TagBlock`] per vendored
//! specification:
//!
//! - [`TIFF6`] — every tag TIFF 6.0 defines (`references/tiff/tiff6.pdf`, Section 8 and the
//!   Part 2 extension sections; Appendix A lists the same 74).
//! - [`DNG`] — every tag DNG 1.7.1.0 Chapter 4 defines (`references/dng/DNG_Spec_1_7_1_0.pdf`).
//!
//! Only blocks with a specification vendored under `references/` are registered. Tags other
//! specifications add to the same number space — TIFF/EP, the Adobe technical notes, XMP, ICC,
//! IPTC, and the vendor lineages exiv2 folds into its "standard" tables — are deliberately absent
//! until their specification is staked; CIPA DC-008's Exif tags live in `gamut-exif`, which owns
//! the profile that defines them.
//!
//! Each row records what a *conformant writer* must emit. A reader is not bound by it: real files
//! carry other types and counts, and nothing in this crate rejects them.

use TagCount::{Any, Exact};

use crate::FieldType;
use crate::FieldType::{Ascii, Byte, Double, Float, Long, Rational, SRational, Short, Undefined};

/// `SubIFDs` (330, 0x014A) — offsets of child image directories (TIFF/EP and DNG raw,
/// reduced-resolution, and mask subfiles).
pub const SUB_IFDS: u16 = 330;

/// `ExifIFD` (34665, 0x8769) — the offset of the Exif private sub-IFD (Exif 3.0 §4.6.3).
pub const EXIF_IFD: u16 = 34665;

/// `GPSInfo` (34853, 0x8825) — the offset of the GPS private sub-IFD.
pub const GPS_INFO: u16 = 34853;

/// `MakerNote` (37500, 0x927C) — an `UNDEFINED` vendor blob, usually a mini-IFD whose internal
/// offsets are relative to the note start or the enclosing TIFF header.
///
/// **Not** a pointer tag for [`read_tree`](crate::read_tree): its value is bytes, not a `LONG`
/// offset array. Decode it by hand with
/// [`IfdReader::with_layout`](crate::IfdReader::with_layout) over a
/// [`Rebased`](crate::Rebased) source once the vendor's offset convention is known.
pub const MAKER_NOTE: u16 = 37500;

/// `InteroperabilityIFD` (40965, 0xA005) — the offset of the Exif Interoperability sub-IFD
/// (reached from the Exif sub-IFD, not the 0th IFD).
pub const INTEROPERABILITY_IFD: u16 = 40965;

/// The standard pointer tags a TIFF/EP-, DNG-, or EXIF-shaped consumer follows with
/// [`read_tree`](crate::read_tree): [`SUB_IFDS`], [`EXIF_IFD`], [`GPS_INFO`], and
/// [`INTEROPERABILITY_IFD`]. ([`MAKER_NOTE`] is deliberately absent — see its docs.)
pub const STANDARD_POINTER_TAGS: &[u16] = &[SUB_IFDS, EXIF_IFD, GPS_INFO, INTEROPERABILITY_IFD];

// The data-locating tag pairs below are the second structural exception (with the pointer tags
// above): their values are `(offset, byte count)` pairs naming ranges of file bytes outside the
// directory/value structure. The byte-completeness audit (`audit`, issue #263) must know them to
// classify those ranges; what the located bytes *mean* stays codec business.

/// `StripOffsets` (273, 0x0111) — the file offsets of the image's strips (TIFF 6.0 §3).
pub const STRIP_OFFSETS: u16 = 273;

/// `StripByteCounts` (279, 0x0117) — the byte count of each strip.
pub const STRIP_BYTE_COUNTS: u16 = 279;

/// `TileOffsets` (324, 0x0144) — the file offsets of the image's tiles (TIFF 6.0 §15).
pub const TILE_OFFSETS: u16 = 324;

/// `TileByteCounts` (325, 0x0145) — the byte count of each tile.
pub const TILE_BYTE_COUNTS: u16 = 325;

/// `FreeOffsets` (288, 0x0120) — the file offsets of declared dead space (TIFF 6.0 §8).
pub const FREE_OFFSETS: u16 = 288;

/// `FreeByteCounts` (289, 0x0121) — the byte count of each free range.
pub const FREE_BYTE_COUNTS: u16 = 289;

/// `JPEGInterchangeFormat` (513, 0x0201) — the file offset of an embedded JPEG stream (an Exif
/// thumbnail, or old-style JPEG-in-TIFF).
pub const JPEG_INTERCHANGE_FORMAT: u16 = 513;

/// `JPEGInterchangeFormatLength` (514, 0x0202) — the embedded JPEG stream's byte count.
pub const JPEG_INTERCHANGE_FORMAT_LENGTH: u16 = 514;

// ---------------------------------------------------------------------------------------------
// The tag registry.
// ---------------------------------------------------------------------------------------------

/// How many components a specification requires a tag's value to have — the `N` / `Count` entry
/// of the clause that defines the tag.
///
/// For the string types (`ASCII`, and the Exif 3.0 `UTF8`) a count is a **byte** count
/// *including* the terminating NUL, matching [`Value::count`](crate::Value::count): `DateTime`'s
/// count of 20 is the 19 characters of `YYYY:MM:DD HH:MM:SS` plus the NUL.
///
/// Ask [`TagCount::allows`] rather than matching the variants — it is the whole of the behaviour
/// and stays correct as variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TagCount {
    /// Exactly this many components.
    Exact(u32),
    /// Any component count: a specification's `Any` (or no count at all), and every count that
    /// depends on another field — TIFF 6.0's `StripsPerImage` and `SamplesPerPixel`, DNG's
    /// `ColorPlanes` — which no fixed number can express.
    Any,
    /// One of a fixed set of counts, in ascending order — CIPA DC-008's `SubjectArea`, "2 or 3
    /// or 4".
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

/// Spells the requirement the way the specifications' count columns do: `20`, `Any`,
/// `2 or 3 or 4`.
impl core::fmt::Display for TagCount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TagCount::Any => f.write_str("Any"),
            TagCount::Exact(n) => write!(f, "{n}"),
            TagCount::OneOf(ns) => {
                for (i, n) in ns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" or ")?;
                    }
                    write!(f, "{n}")?;
                }
                Ok(())
            }
        }
    }
}

/// One registered tag: its number, the name its specification gives it, and the value shape that
/// specification mandates.
///
/// `#[non_exhaustive]`, so a column can be added without a breaking change; rows are built only
/// by this crate, and read through the public fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TagSpec {
    /// The 16-bit on-disk tag number.
    pub id: u16,
    /// The name the defining specification gives the tag, spelled as its clause heading does.
    pub name: &'static str,
    /// The field types the specification permits, in the order it lists them (`SHORT or LONG`
    /// reads as `[Short, Long]`). **Empty** where it permits any type (TIFF 6.0's
    /// `SMinSampleValue`: "the field type that best matches the sample data").
    pub field_types: &'static [FieldType],
    /// The component count the specification requires.
    pub count: TagCount,
}

/// Builds one registry row; `const` so the blocks below are plain data.
const fn row(
    id: u16,
    name: &'static str,
    field_types: &'static [FieldType],
    count: TagCount,
) -> TagSpec {
    TagSpec {
        id,
        name,
        field_types,
        count,
    }
}

/// The tags one vendored specification defines, in ascending tag-number order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagBlock {
    specification: &'static str,
    specs: &'static [TagSpec],
}

impl TagBlock {
    /// The specification the rows are transcribed from, with the path it is vendored at.
    #[must_use]
    pub const fn specification(self) -> &'static str {
        self.specification
    }

    /// Every row, in ascending tag-number order.
    #[must_use]
    pub const fn specs(self) -> &'static [TagSpec] {
        self.specs
    }

    /// The row for tag `id`, or `None` if this specification does not define it.
    #[must_use]
    pub fn find(self, id: u16) -> Option<&'static TagSpec> {
        self.specs.iter().find(|spec| spec.id == id)
    }
}

/// Every tag **TIFF 6.0** defines (`references/tiff/tiff6.pdf`, Adobe, Final — June 3 1992).
///
/// Each row is transcribed from the clause that defines the tag — Section 8 for the baseline
/// fields, the Part 2 section for an extension — rather than from Appendix A, which lists the same
/// 74 tags but disagrees with the defining clause in three places: `ExtraSamples` (Appendix A
/// `BYTE`, Section 8 `SHORT`), `ReferenceBlackWhite` (Appendix A `LONG`, `2*SamplesPerPixel`;
/// Section 20 `RATIONAL`, 6) and `JPEGInterchangeFormatLength` (Appendix A abbreviates it
/// `JPEGInterchangeFormatLngth`). A field with no `N` in its clause takes [`TagCount::Any`].
pub const TIFF6: TagBlock = TagBlock {
    specification: "TIFF 6.0 (references/tiff/tiff6.pdf)",
    specs: &[
        // Section 8 "Baseline Field Reference Guide", except where a row names its section.
        row(254, "NewSubfileType", &[Long], Exact(1)),
        row(255, "SubfileType", &[Short], Exact(1)),
        row(256, "ImageWidth", &[Short, Long], Exact(1)),
        row(257, "ImageLength", &[Short, Long], Exact(1)),
        // N = SamplesPerPixel.
        row(258, "BitsPerSample", &[Short], Any),
        row(259, "Compression", &[Short], Exact(1)),
        row(262, "PhotometricInterpretation", &[Short], Exact(1)),
        row(263, "Threshholding", &[Short], Exact(1)),
        row(264, "CellWidth", &[Short], Exact(1)),
        row(265, "CellLength", &[Short], Exact(1)),
        row(266, "FillOrder", &[Short], Exact(1)),
        // Section 12 "Document Storage and Retrieval".
        row(269, "DocumentName", &[Ascii], Any),
        row(270, "ImageDescription", &[Ascii], Any),
        row(271, "Make", &[Ascii], Any),
        row(272, "Model", &[Ascii], Any),
        // N = StripsPerImage (times SamplesPerPixel when planar).
        row(273, "StripOffsets", &[Short, Long], Any),
        row(274, "Orientation", &[Short], Exact(1)),
        row(277, "SamplesPerPixel", &[Short], Exact(1)),
        row(278, "RowsPerStrip", &[Short, Long], Exact(1)),
        row(279, "StripByteCounts", &[Short, Long], Any),
        row(280, "MinSampleValue", &[Short], Any),
        row(281, "MaxSampleValue", &[Short], Any),
        row(282, "XResolution", &[Rational], Exact(1)),
        row(283, "YResolution", &[Rational], Exact(1)),
        row(284, "PlanarConfiguration", &[Short], Exact(1)),
        // Section 12 "Document Storage and Retrieval".
        row(285, "PageName", &[Ascii], Any),
        row(286, "XPosition", &[Rational], Exact(1)),
        row(287, "YPosition", &[Rational], Exact(1)),
        row(288, "FreeOffsets", &[Long], Any),
        row(289, "FreeByteCounts", &[Long], Any),
        row(290, "GrayResponseUnit", &[Short], Exact(1)),
        // N = 2**BitsPerSample.
        row(291, "GrayResponseCurve", &[Short], Any),
        // Section 11 "CCITT Bilevel Encodings".
        row(292, "T4Options", &[Long], Exact(1)),
        row(293, "T6Options", &[Long], Exact(1)),
        row(296, "ResolutionUnit", &[Short], Exact(1)),
        // Section 12 "Document Storage and Retrieval".
        row(297, "PageNumber", &[Short], Exact(2)),
        // Section 20 "RGB Image Colorimetry": N = {1 or 3} * (1 << BitsPerSample).
        row(301, "TransferFunction", &[Short], Any),
        row(305, "Software", &[Ascii], Any),
        row(306, "DateTime", &[Ascii], Exact(20)),
        row(315, "Artist", &[Ascii], Any),
        row(316, "HostComputer", &[Ascii], Any),
        // Section 14 "Differencing Predictor".
        row(317, "Predictor", &[Short], Exact(1)),
        // Section 20 "RGB Image Colorimetry".
        row(318, "WhitePoint", &[Rational], Exact(2)),
        row(319, "PrimaryChromaticities", &[Rational], Exact(6)),
        // N = 3 * (2**BitsPerSample).
        row(320, "ColorMap", &[Short], Any),
        // Section 17 "HalftoneHints".
        row(321, "HalftoneHints", &[Short], Exact(2)),
        // Section 15 "Tiled Images"; the offsets and counts are N = TilesPerImage.
        row(322, "TileWidth", &[Short, Long], Exact(1)),
        row(323, "TileLength", &[Short, Long], Exact(1)),
        row(324, "TileOffsets", &[Long], Any),
        row(325, "TileByteCounts", &[Short, Long], Any),
        // Section 16 "CMYK Images".
        row(332, "InkSet", &[Short], Exact(1)),
        // N = the bytes of every ink name, NULs included.
        row(333, "InkNames", &[Ascii], Any),
        row(334, "NumberOfInks", &[Short], Exact(1)),
        // N = 2, or 2*SamplesPerPixel.
        row(336, "DotRange", &[Byte, Short], Any),
        row(337, "TargetPrinter", &[Ascii], Any),
        // Section 8 gives `SHORT`, N = m (the number of extra components); Appendix A's `BYTE`
        // contradicts the defining clause.
        row(338, "ExtraSamples", &[Short], Any),
        // Section 19 "Data Sample Format"; N = SamplesPerPixel. The two sample bounds take "the
        // field type that best matches the sample data", so no type is listed.
        row(339, "SampleFormat", &[Short], Any),
        row(340, "SMinSampleValue", &[], Any),
        row(341, "SMaxSampleValue", &[], Any),
        // Section 20 "RGB Image Colorimetry".
        row(342, "TransferRange", &[Short], Exact(6)),
        // Section 22 "JPEG Compression"; the per-component tables are N = SamplesPerPixel.
        row(512, "JPEGProc", &[Short], Exact(1)),
        row(513, "JPEGInterchangeFormat", &[Long], Exact(1)),
        row(514, "JPEGInterchangeFormatLength", &[Long], Exact(1)),
        row(515, "JPEGRestartInterval", &[Short], Exact(1)),
        row(517, "JPEGLosslessPredictors", &[Short], Any),
        row(518, "JPEGPointTransforms", &[Short], Any),
        row(519, "JPEGQTables", &[Long], Any),
        row(520, "JPEGDCTables", &[Long], Any),
        row(521, "JPEGACTables", &[Long], Any),
        // Section 21 "YCbCr Images".
        row(529, "YCbCrCoefficients", &[Rational], Exact(3)),
        row(530, "YCbCrSubSampling", &[Short], Exact(2)),
        row(531, "YCbCrPositioning", &[Short], Exact(1)),
        // Section 20 "RGB Image Colorimetry" gives `RATIONAL`, N = 6; Appendix A's `LONG`,
        // `2*SamplesPerPixel` contradicts the defining clause.
        row(532, "ReferenceBlackWhite", &[Rational], Exact(6)),
        row(33432, "Copyright", &[Ascii], Any),
    ],
};

/// Every tag **DNG 1.7.1.0** defines (`references/dng/DNG_Spec_1_7_1_0.pdf`, Adobe, September
/// 2023): Chapter 4 "DNG Tags" and its "Additional Tags for Version 1.x" sections.
///
/// Each row is the `Tag` / `Type` / `Count` table that heads the tag's clause. A count expressed
/// through other fields (`ColorPlanes`, `SamplesPerPixel`, "String length including null",
/// "Byte count of data") takes [`TagCount::Any`]. The baseline TIFF and TIFF/EP tags DNG uses
/// are not repeated here: Chapter 3 restricts some of them, but defines none.
pub const DNG: TagBlock = TagBlock {
    specification: "DNG 1.7.1.0 (references/dng/DNG_Spec_1_7_1_0.pdf)",
    specs: &[
        // Chapter 4 "DNG Tags" (DNG 1.0 / 1.1).
        row(50706, "DNGVersion", &[Byte], Exact(4)),
        row(50707, "DNGBackwardVersion", &[Byte], Exact(4)),
        row(50708, "UniqueCameraModel", &[Ascii], Any),
        row(50709, "LocalizedCameraModel", &[Ascii, Byte], Any),
        row(50710, "CFAPlaneColor", &[Byte], Any),
        row(50711, "CFALayout", &[Short], Exact(1)),
        row(50712, "LinearizationTable", &[Short], Any),
        row(50713, "BlackLevelRepeatDim", &[Short], Exact(2)),
        row(50714, "BlackLevel", &[Short, Long, Rational], Any),
        row(50715, "BlackLevelDeltaH", &[SRational], Any),
        row(50716, "BlackLevelDeltaV", &[SRational], Any),
        row(50717, "WhiteLevel", &[Short, Long], Any),
        row(50718, "DefaultScale", &[Rational], Exact(2)),
        row(
            50719,
            "DefaultCropOrigin",
            &[Short, Long, Rational],
            Exact(2),
        ),
        row(50720, "DefaultCropSize", &[Short, Long, Rational], Exact(2)),
        row(50721, "ColorMatrix1", &[SRational], Any),
        row(50722, "ColorMatrix2", &[SRational], Any),
        row(50723, "CameraCalibration1", &[SRational], Any),
        row(50724, "CameraCalibration2", &[SRational], Any),
        row(50725, "ReductionMatrix1", &[SRational], Any),
        row(50726, "ReductionMatrix2", &[SRational], Any),
        row(50727, "AnalogBalance", &[Rational], Any),
        row(50728, "AsShotNeutral", &[Short, Rational], Any),
        row(50729, "AsShotWhiteXY", &[Rational], Exact(2)),
        row(50730, "BaselineExposure", &[SRational], Exact(1)),
        row(50731, "BaselineNoise", &[Rational], Exact(1)),
        row(50732, "BaselineSharpness", &[Rational], Exact(1)),
        row(50733, "BayerGreenSplit", &[Long], Exact(1)),
        row(50734, "LinearResponseLimit", &[Rational], Exact(1)),
        row(50735, "CameraSerialNumber", &[Ascii], Any),
        row(50736, "LensInfo", &[Rational], Exact(4)),
        row(50737, "ChromaBlurRadius", &[Rational], Exact(1)),
        row(50738, "AntiAliasStrength", &[Rational], Exact(1)),
        row(50739, "ShadowScale", &[Rational], Exact(1)),
        row(50740, "DNGPrivateData", &[Byte], Any),
        row(50741, "MakerNoteSafety", &[Short], Exact(1)),
        row(50778, "CalibrationIlluminant1", &[Short], Exact(1)),
        row(50779, "CalibrationIlluminant2", &[Short], Exact(1)),
        row(50780, "BestQualityScale", &[Rational], Exact(1)),
        row(50781, "RawDataUniqueID", &[Byte], Exact(16)),
        row(50827, "OriginalRawFileName", &[Ascii, Byte], Any),
        row(50828, "OriginalRawFileData", &[Undefined], Any),
        row(50829, "ActiveArea", &[Short, Long], Exact(4)),
        row(50830, "MaskedAreas", &[Short, Long], Any),
        row(50831, "AsShotICCProfile", &[Undefined], Any),
        row(50832, "AsShotPreProfileMatrix", &[SRational], Any),
        row(50833, "CurrentICCProfile", &[Undefined], Any),
        row(50834, "CurrentPreProfileMatrix", &[SRational], Any),
        // "Additional Tags for Version 1.2.0.0".
        row(50879, "ColorimetricReference", &[Short], Exact(1)),
        row(50931, "CameraCalibrationSignature", &[Ascii, Byte], Any),
        row(50932, "ProfileCalibrationSignature", &[Ascii, Byte], Any),
        row(50933, "ExtraCameraProfiles", &[Long], Any),
        row(50934, "AsShotProfileName", &[Ascii, Byte], Any),
        row(50935, "NoiseReductionApplied", &[Rational], Exact(1)),
        row(50936, "ProfileName", &[Ascii, Byte], Any),
        row(50937, "ProfileHueSatMapDims", &[Long], Exact(3)),
        row(50938, "ProfileHueSatMapData1", &[Float], Any),
        row(50939, "ProfileHueSatMapData2", &[Float], Any),
        row(50940, "ProfileToneCurve", &[Float], Any),
        row(50941, "ProfileEmbedPolicy", &[Long], Exact(1)),
        row(50942, "ProfileCopyright", &[Ascii, Byte], Any),
        row(50964, "ForwardMatrix1", &[SRational], Any),
        row(50965, "ForwardMatrix2", &[SRational], Any),
        row(50966, "PreviewApplicationName", &[Ascii, Byte], Any),
        row(50967, "PreviewApplicationVersion", &[Ascii, Byte], Any),
        row(50968, "PreviewSettingsName", &[Ascii, Byte], Any),
        row(50969, "PreviewSettingsDigest", &[Byte], Exact(16)),
        row(50970, "PreviewColorSpace", &[Long], Exact(1)),
        row(50971, "PreviewDateTime", &[Ascii], Any),
        row(50972, "RawImageDigest", &[Byte], Exact(16)),
        row(50973, "OriginalRawFileDigest", &[Byte], Exact(16)),
        row(50974, "SubTileBlockSize", &[Short, Long], Exact(2)),
        row(50975, "RowInterleaveFactor", &[Short, Long], Exact(1)),
        row(50981, "ProfileLookTableDims", &[Long], Exact(3)),
        row(50982, "ProfileLookTableData", &[Float], Any),
        // "Additional Tags for Version 1.3.0.0".
        row(51008, "OpcodeList1", &[Undefined], Any),
        row(51009, "OpcodeList2", &[Undefined], Any),
        row(51022, "OpcodeList3", &[Undefined], Any),
        // Count "2 or 2 * ColorPlanes".
        row(51041, "NoiseProfile", &[Double], Any),
        // "Additional Tags for Version 1.4.0.0".
        row(51089, "OriginalDefaultFinalSize", &[Short, Long], Exact(2)),
        row(
            51090,
            "OriginalBestQualityFinalSize",
            &[Short, Long],
            Exact(2),
        ),
        row(
            51091,
            "OriginalDefaultCropSize",
            &[Short, Long, Rational],
            Exact(2),
        ),
        row(51107, "ProfileHueSatMapEncoding", &[Long], Exact(1)),
        row(51108, "ProfileLookTableEncoding", &[Long], Exact(1)),
        row(51109, "BaselineExposureOffset", &[Rational], Exact(1)),
        row(51110, "DefaultBlackRender", &[Long], Exact(1)),
        row(51111, "NewRawImageDigest", &[Byte], Exact(16)),
        row(51112, "RawToPreviewGain", &[Double], Exact(1)),
        row(51125, "DefaultUserCrop", &[Rational], Exact(4)),
        // "Additional Tags for Version 1.5.0.0".
        row(51177, "DepthFormat", &[Short], Exact(1)),
        row(51178, "DepthNear", &[Rational], Exact(1)),
        row(51179, "DepthFar", &[Rational], Exact(1)),
        row(51180, "DepthUnits", &[Short], Exact(1)),
        row(51181, "DepthMeasureType", &[Short], Exact(1)),
        row(51182, "EnhanceParams", &[Ascii], Any),
        // "Additional Tags for Version 1.6.0.0".
        row(52525, "ProfileGainTableMap", &[Undefined], Any),
        row(52526, "SemanticName", &[Ascii], Any),
        row(52528, "SemanticInstanceID", &[Ascii], Any),
        row(52529, "CalibrationIlluminant3", &[Short], Exact(1)),
        row(52530, "CameraCalibration3", &[SRational], Any),
        row(52531, "ColorMatrix3", &[SRational], Any),
        row(52532, "ForwardMatrix3", &[SRational], Any),
        row(52533, "IlluminantData1", &[Undefined], Any),
        row(52534, "IlluminantData2", &[Undefined], Any),
        row(52535, "IlluminantData3", &[Undefined], Any),
        row(52536, "MaskSubArea", &[Long], Exact(4)),
        row(52537, "ProfileHueSatMapData3", &[Float], Any),
        row(52538, "ReductionMatrix3", &[SRational], Any),
        row(52543, "RGBTables", &[Undefined], Any),
        // "Additional Tags for Version 1.7.0.0".
        row(52544, "ProfileGainTableMap2", &[Undefined], Any),
        // "Additional Tags for Version 1.7.1.0".
        row(52547, "ColumnInterleaveFactor", &[Short, Long], Exact(1)),
        // "Additional Tags for Version 1.7.0.0".
        row(52548, "ImageSequenceInfo", &[Undefined], Any),
        row(52550, "ImageStats", &[Undefined], Any),
        row(52551, "ProfileDynamicRange", &[Undefined], Exact(8)),
        row(52552, "ProfileGroupName", &[Ascii, Byte], Any),
        // "Additional Tags for Version 1.7.1.0".
        row(52553, "JXLDistance", &[Float], Exact(1)),
        row(52554, "JXLEffort", &[Long], Exact(1)),
        row(52555, "JXLDecodeSpeed", &[Long], Exact(1)),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the constants to the spec-assigned numbers (TIFF/EP, Exif 3.0) and the pointer set
    /// to exactly the followable four.
    #[test]
    fn structural_tags_match_the_specs() {
        assert_eq!(SUB_IFDS, 0x014A);
        assert_eq!(EXIF_IFD, 0x8769);
        assert_eq!(GPS_INFO, 0x8825);
        assert_eq!(MAKER_NOTE, 0x927C);
        assert_eq!(INTEROPERABILITY_IFD, 0xA005);
        assert_eq!(
            STANDARD_POINTER_TAGS,
            &[SUB_IFDS, EXIF_IFD, GPS_INFO, INTEROPERABILITY_IFD]
        );
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
    fn count_displays_as_the_spec_writes_it() {
        // The count columns' three spellings, which a write-side error message quotes.
        assert_eq!(TagCount::Exact(20).to_string(), "20");
        assert_eq!(TagCount::Any.to_string(), "Any");
        assert_eq!(TagCount::OneOf(&[2, 3, 4]).to_string(), "2 or 3 or 4");
        assert_eq!(TagCount::OneOf(&[2]).to_string(), "2");
        assert_eq!(TagCount::OneOf(&[]).to_string(), "");
    }

    #[test]
    fn each_block_names_its_vendored_specification() {
        assert_eq!(
            TIFF6.specification(),
            "TIFF 6.0 (references/tiff/tiff6.pdf)"
        );
        assert_eq!(
            DNG.specification(),
            "DNG 1.7.1.0 (references/dng/DNG_Spec_1_7_1_0.pdf)"
        );
    }

    #[test]
    fn each_block_holds_every_tag_its_specification_defines() {
        // TIFF 6.0 Appendix A lists 74 tags; DNG 1.7.1.0 gives 118 a `Tag`/`Type`/`Count` table.
        assert_eq!(TIFF6.specs().len(), 74);
        assert_eq!(DNG.specs().len(), 118);
    }

    #[test]
    fn rows_ascend_strictly_so_no_id_is_registered_twice() {
        for block in [TIFF6, DNG] {
            assert!(
                block.specs().windows(2).all(|w| w[0].id < w[1].id),
                "{} is out of order or repeats an id",
                block.specification()
            );
        }
    }

    #[test]
    fn find_returns_each_row_by_its_id() {
        for block in [TIFF6, DNG] {
            for spec in block.specs() {
                assert_eq!(block.find(spec.id), Some(spec));
            }
        }
    }

    #[test]
    fn find_rejects_an_id_the_specification_does_not_define() {
        // 516 sits inside TIFF 6.0's JPEG run (512..=521) but is unassigned.
        assert_eq!(TIFF6.find(516), None);
        // Each block answers for its own specification only.
        assert_eq!(TIFF6.find(50706), None);
        assert_eq!(DNG.find(256), None);
        // 52527 sits inside DNG 1.6's run but is unassigned.
        assert_eq!(DNG.find(52527), None);
    }

    #[test]
    fn structural_constants_carry_the_names_tiff6_gives_them() {
        for (id, name) in [
            (STRIP_OFFSETS, "StripOffsets"),
            (STRIP_BYTE_COUNTS, "StripByteCounts"),
            (TILE_OFFSETS, "TileOffsets"),
            (TILE_BYTE_COUNTS, "TileByteCounts"),
            (FREE_OFFSETS, "FreeOffsets"),
            (FREE_BYTE_COUNTS, "FreeByteCounts"),
            (JPEG_INTERCHANGE_FORMAT, "JPEGInterchangeFormat"),
            (
                JPEG_INTERCHANGE_FORMAT_LENGTH,
                "JPEGInterchangeFormatLength",
            ),
        ] {
            assert_eq!(TIFF6.find(id).map(|s| s.name), Some(name));
        }
    }

    #[test]
    fn tiff6_rows_follow_the_defining_clause_not_appendix_a() {
        // The three places Appendix A disagrees with the clause that defines the tag; this pins the
        // reading so nobody "corrects" a row back to the summary table.
        let extra = TIFF6.find(338).unwrap();
        assert_eq!(extra.field_types, &[FieldType::Short]);
        let rbw = TIFF6.find(532).unwrap();
        assert_eq!(rbw.field_types, &[FieldType::Rational]);
        assert_eq!(rbw.count, TagCount::Exact(6));
        assert_eq!(TIFF6.find(514).unwrap().name, "JPEGInterchangeFormatLength");
    }

    #[test]
    fn rows_pin_one_tag_of_each_shape() {
        // A fixed count, a string count with its NUL, an open count, a type alternative in spec
        // order, and a type left open.
        assert_eq!(TIFF6.find(297).unwrap().count, TagCount::Exact(2));
        assert_eq!(TIFF6.find(306).unwrap().count, TagCount::Exact(20));
        assert_eq!(TIFF6.find(273).unwrap().count, TagCount::Any);
        assert_eq!(
            DNG.find(50714).unwrap().field_types,
            &[FieldType::Short, FieldType::Long, FieldType::Rational]
        );
        assert!(TIFF6.find(340).unwrap().field_types.is_empty());
        assert_eq!(DNG.find(52551).unwrap().count, TagCount::Exact(8));
    }

    #[test]
    fn no_row_lists_a_type_twice() {
        // A duplicated type would be a transcription slip no other assertion would notice.
        for block in [TIFF6, DNG] {
            for spec in block.specs() {
                let types = spec.field_types;
                for (i, t) in types.iter().enumerate() {
                    assert!(!types[i + 1..].contains(t), "{} repeats a type", spec.name);
                }
            }
        }
    }
}
