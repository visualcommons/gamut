//! Enumerated DNG tag *values* — the codes stored in the `CalibrationIlluminant`, `CFALayout`,
//! `ProfileEmbedPolicy`, `PreviewColorSpace`, and `TableEncoding` tags, plus the
//! `NewSubFileType` bit values.
//!
//! Values come from the **DNG 1.7.1.0** specification and the Adobe DNG SDK's `dng_tag_values.h`.
//! Each enum follows gamut-ifd's tag-value idiom: an explicit `#[repr]` of the tag's own on-disk
//! type (`u16` for a `SHORT` tag, `u32` for a `LONG` one) with the code as the permanent
//! discriminant, read with one `TryFrom` that refuses an unassigned code and written with
//! `u16::from` / `u32::from`.
//!
//! The `Compression`, `PhotometricInterpretation`, `Predictor` and `SampleFormat` codes are
//! gamut-ifd's, shared with gamut-tiff and re-exported here: each is a `#[repr(u16)]` enum read
//! with `TryFrom<u16>` and written with `u16::from`. Their union carries codes only TIFF assigns
//! (CCITT, LZW and PackBits compression, the palette and CMYK photometries, …), and what stays
//! here is which of them a DNG reader admits. What gamut-dng does with a `Compression` is asked
//! through the free functions [`is_decodable`](crate::is_decodable) and
//! [`is_lossy`](crate::is_lossy), which replace the inherent methods of the same names the
//! crate-local enum carried.

pub use gamut_ifd::tags::{Compression, PhotometricInterpretation, Predictor, SampleFormat};

/// Reads an on-disk `Compression` code (tag 259) as one of the five DNG 1.7.1.0 assigns —
/// uncompressed, JPEG, Deflate, lossy JPEG and JPEG XL — or `None`.
///
/// Only a scheme's own code is admitted: the legacy Adobe Deflate alias `32946`, which gamut-ifd
/// reads as [`Compression::Deflate`] for TIFF, is not DNG's.
pub(crate) fn dng_compression(code: u16) -> Option<Compression> {
    Compression::try_from(code).ok().filter(|&compression| {
        u16::from(compression) == code
            && matches!(
                compression,
                Compression::None
                    | Compression::Jpeg
                    | Compression::Deflate
                    | Compression::LossyJpeg
                    | Compression::JpegXl
            )
    })
}

/// Reads an on-disk `PhotometricInterpretation` code (tag 262) as one DNG 1.7.1.0 assigns, or
/// `None` — the photometries of DNG's raw images, previews and masks.
pub(crate) fn dng_photometric(code: u16) -> Option<PhotometricInterpretation> {
    PhotometricInterpretation::try_from(code)
        .ok()
        .filter(|photometric| {
            matches!(
                photometric,
                PhotometricInterpretation::BlackIsZero
                    | PhotometricInterpretation::Rgb
                    | PhotometricInterpretation::TransparencyMask
                    | PhotometricInterpretation::YCbCr
                    | PhotometricInterpretation::Cfa
                    | PhotometricInterpretation::LinearRaw
                    | PhotometricInterpretation::Depth
                    | PhotometricInterpretation::PhotometricMask
            )
        })
}

/// The `unsupported` error every `TryFrom` below returns for an unassigned code.
fn unassigned(message: &'static str) -> gamut_core::Error {
    gamut_core::Error::unsupported(env!("CARGO_PKG_NAME"), message)
}

/// A calibration illuminant, stored in `CalibrationIlluminant1/2/3` (and equal to the EXIF
/// `LightSource` codes).
///
/// [`Other`](Self::Other) (`255`) signals that spectral data is supplied separately via the
/// `IlluminantData1/2/3` tags instead of a named illuminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
#[non_exhaustive]
pub enum CalibrationIlluminant {
    /// `0` — unknown.
    Unknown = 0,
    /// `1` — daylight.
    Daylight = 1,
    /// `2` — fluorescent.
    Fluorescent = 2,
    /// `3` — tungsten (incandescent).
    Tungsten = 3,
    /// `4` — flash.
    Flash = 4,
    /// `9` — fine weather.
    FineWeather = 9,
    /// `10` — cloudy weather.
    CloudyWeather = 10,
    /// `11` — shade.
    Shade = 11,
    /// `12` — daylight fluorescent (D, 5700–7100 K).
    DaylightFluorescent = 12,
    /// `13` — day-white fluorescent (N, 4600–5500 K).
    DayWhiteFluorescent = 13,
    /// `14` — cool-white fluorescent (W, 3800–4500 K).
    CoolWhiteFluorescent = 14,
    /// `15` — white fluorescent (WW, 3250–3800 K).
    WhiteFluorescent = 15,
    /// `16` — warm-white fluorescent (L, 2600–3250 K).
    WarmWhiteFluorescent = 16,
    /// `17` — Standard Light A.
    StandardLightA = 17,
    /// `18` — Standard Light B.
    StandardLightB = 18,
    /// `19` — Standard Light C.
    StandardLightC = 19,
    /// `20` — D55.
    D55 = 20,
    /// `21` — D65.
    D65 = 21,
    /// `22` — D75.
    D75 = 22,
    /// `23` — D50.
    D50 = 23,
    /// `24` — ISO studio tungsten.
    IsoStudioTungsten = 24,
    /// `255` — other (spectral data given via `IlluminantData`).
    Other = 255,
}

impl TryFrom<u16> for CalibrationIlluminant {
    type Error = gamut_core::Error;

    /// Maps an on-disk `CalibrationIlluminant` code to its illuminant.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code the
    /// specification does not assign.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => CalibrationIlluminant::Unknown,
            1 => CalibrationIlluminant::Daylight,
            2 => CalibrationIlluminant::Fluorescent,
            3 => CalibrationIlluminant::Tungsten,
            4 => CalibrationIlluminant::Flash,
            9 => CalibrationIlluminant::FineWeather,
            10 => CalibrationIlluminant::CloudyWeather,
            11 => CalibrationIlluminant::Shade,
            12 => CalibrationIlluminant::DaylightFluorescent,
            13 => CalibrationIlluminant::DayWhiteFluorescent,
            14 => CalibrationIlluminant::CoolWhiteFluorescent,
            15 => CalibrationIlluminant::WhiteFluorescent,
            16 => CalibrationIlluminant::WarmWhiteFluorescent,
            17 => CalibrationIlluminant::StandardLightA,
            18 => CalibrationIlluminant::StandardLightB,
            19 => CalibrationIlluminant::StandardLightC,
            20 => CalibrationIlluminant::D55,
            21 => CalibrationIlluminant::D65,
            22 => CalibrationIlluminant::D75,
            23 => CalibrationIlluminant::D50,
            24 => CalibrationIlluminant::IsoStudioTungsten,
            255 => CalibrationIlluminant::Other,
            _ => return Err(unassigned("unrecognised CalibrationIlluminant code")),
        })
    }
}

impl From<CalibrationIlluminant> for u16 {
    /// The on-disk code, the `SHORT` written to `CalibrationIlluminant1/2/3`.
    fn from(illuminant: CalibrationIlluminant) -> Self {
        illuminant as u16
    }
}

/// The physical layout of the colour filter array, stored in `CFALayout` (50711).
///
/// `1` is the common rectangular (square) grid (e.g. ordinary Bayer); `2`–`9` are the staggered
/// layouts where alternate rows or columns are offset by half a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u16)]
#[non_exhaustive]
pub enum CfaLayout {
    /// `1` — rectangular (square) layout.
    #[default]
    Rectangular = 1,
    /// `2` — staggered layout A (even columns shifted down by 1/2 row).
    StaggeredA = 2,
    /// `3` — staggered layout B (even columns shifted up by 1/2 row).
    StaggeredB = 3,
    /// `4` — staggered layout C (even rows shifted right by 1/2 column).
    StaggeredC = 4,
    /// `5` — staggered layout D (even rows shifted left by 1/2 column).
    StaggeredD = 5,
    /// `6` — staggered layout E.
    StaggeredE = 6,
    /// `7` — staggered layout F.
    StaggeredF = 7,
    /// `8` — staggered layout G.
    StaggeredG = 8,
    /// `9` — staggered layout H.
    StaggeredH = 9,
}

impl TryFrom<u16> for CfaLayout {
    type Error = gamut_core::Error;

    /// Maps an on-disk `CFALayout` code to its layout.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code outside
    /// `1..=9`.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => CfaLayout::Rectangular,
            2 => CfaLayout::StaggeredA,
            3 => CfaLayout::StaggeredB,
            4 => CfaLayout::StaggeredC,
            5 => CfaLayout::StaggeredD,
            6 => CfaLayout::StaggeredE,
            7 => CfaLayout::StaggeredF,
            8 => CfaLayout::StaggeredG,
            9 => CfaLayout::StaggeredH,
            _ => return Err(unassigned("unrecognised CFALayout code")),
        })
    }
}

impl From<CfaLayout> for u16 {
    /// The on-disk code, the `SHORT` written to `CFALayout`.
    fn from(layout: CfaLayout) -> Self {
        layout as u16
    }
}

/// The embedding/usage policy of a camera profile, stored in `ProfileEmbedPolicy` (50941).
///
/// `#[repr(u32)]`: the tag is a `LONG`, so its code is the 32-bit value on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
#[non_exhaustive]
pub enum ProfileEmbedPolicy {
    /// `0` — the profile may be freely copied.
    #[default]
    AllowCopying = 0,
    /// `1` — embed the profile only in files it was used to render.
    EmbedIfUsed = 1,
    /// `2` — never embed the profile.
    EmbedNever = 2,
    /// `3` — no restrictions.
    NoRestrictions = 3,
}

impl TryFrom<u32> for ProfileEmbedPolicy {
    type Error = gamut_core::Error;

    /// Maps an on-disk `ProfileEmbedPolicy` code to its policy.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code the
    /// specification does not assign.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => ProfileEmbedPolicy::AllowCopying,
            1 => ProfileEmbedPolicy::EmbedIfUsed,
            2 => ProfileEmbedPolicy::EmbedNever,
            3 => ProfileEmbedPolicy::NoRestrictions,
            _ => return Err(unassigned("unrecognised ProfileEmbedPolicy code")),
        })
    }
}

impl From<ProfileEmbedPolicy> for u32 {
    /// The on-disk code, the `LONG` written to `ProfileEmbedPolicy`.
    fn from(policy: ProfileEmbedPolicy) -> Self {
        policy as u32
    }
}

/// The colour space of a preview image, stored in `PreviewColorSpace` (50970).
///
/// `#[repr(u32)]`: the tag is a `LONG`, so its code is the 32-bit value on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
#[non_exhaustive]
pub enum PreviewColorSpace {
    /// `0` — unknown.
    #[default]
    Unknown = 0,
    /// `1` — grayscale, gamma 2.2.
    GrayGamma22 = 1,
    /// `2` — sRGB.
    Srgb = 2,
    /// `3` — Adobe RGB (1998).
    AdobeRgb = 3,
    /// `4` — ProPhoto RGB.
    ProPhotoRgb = 4,
}

impl TryFrom<u32> for PreviewColorSpace {
    type Error = gamut_core::Error;

    /// Maps an on-disk `PreviewColorSpace` code to its colour space.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code the
    /// specification does not assign.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => PreviewColorSpace::Unknown,
            1 => PreviewColorSpace::GrayGamma22,
            2 => PreviewColorSpace::Srgb,
            3 => PreviewColorSpace::AdobeRgb,
            4 => PreviewColorSpace::ProPhotoRgb,
            _ => return Err(unassigned("unrecognised PreviewColorSpace code")),
        })
    }
}

impl From<PreviewColorSpace> for u32 {
    /// The on-disk code, the `LONG` written to `PreviewColorSpace`.
    fn from(space: PreviewColorSpace) -> Self {
        space as u32
    }
}

/// How a 3D hue/saturation/value table is indexed, stored in `ProfileHueSatMapEncoding` (51107)
/// and `ProfileLookTableEncoding` (51108).
///
/// Both tags default to [`Linear`](Self::Linear) when absent, and neither applies to a 2.5D table
/// (one whose value division count is 1). `#[repr(u32)]`: both tags are `LONG`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
#[non_exhaustive]
pub enum TableEncoding {
    /// `0` — the table is indexed by linear ProPhoto RGB converted straight to HSV.
    #[default]
    Linear = 0,
    /// `1` — the value coordinate is sRGB-encoded before indexing, giving shadows more table
    /// precision.
    Srgb = 1,
}

impl TryFrom<u32> for TableEncoding {
    type Error = gamut_core::Error;

    /// Maps an on-disk table-encoding code to its encoding.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code other
    /// than `0` or `1`.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => TableEncoding::Linear,
            1 => TableEncoding::Srgb,
            _ => return Err(unassigned("unrecognised table encoding code")),
        })
    }
}

impl From<TableEncoding> for u32 {
    /// The on-disk code, the `LONG` written to the table-encoding tags.
    fn from(encoding: TableEncoding) -> Self {
        encoding as u32
    }
}

/// `NewSubFileType` (254) values DNG assigns to each subfile.
///
/// The TIFF field is a 32-bit bit field, but DNG uses a small set of specific values to label what
/// each IFD holds. The main raw image is `0`; a reduced-resolution preview is `1`.
pub mod new_subfile_type {
    /// The main (full-resolution) raw image.
    pub const MAIN_IMAGE: u32 = 0;
    /// A reduced-resolution preview or thumbnail.
    pub const PREVIEW_IMAGE: u32 = 1;
    /// A transparency mask.
    pub const TRANSPARENCY_MASK: u32 = 4;
    /// A depth map.
    pub const DEPTH_MAP: u32 = 8;
    /// An enhanced (e.g. denoised / super-resolution) version of the main image.
    pub const ENHANCED_IMAGE: u32 = 16;
    /// A gain map.
    pub const GAIN_MAP: u32 = 32;
    /// An alternate (non-primary) preview image.
    pub const ALT_PREVIEW_IMAGE: u32 = 0x0001_0001;
    /// A semantic mask.
    pub const SEMANTIC_MASK: u32 = 0x0001_0004;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DNG reads exactly its five schemes, by their own codes; the TIFF-only schemes and the
    /// legacy Deflate alias gamut-ifd reads for TIFF are refused.
    #[test]
    fn dng_compression_reads_only_dngs_codes() {
        for (code, c) in [
            (1, Compression::None),
            (7, Compression::Jpeg),
            (8, Compression::Deflate),
            (34892, Compression::LossyJpeg),
            (52546, Compression::JpegXl),
        ] {
            assert_eq!(dng_compression(code), Some(c), "{code}");
        }
        for bad in [0, 2, 3, 4, 5, 6, 9, 32773, 32946] {
            assert_eq!(dng_compression(bad), None, "{bad}");
        }
    }

    /// DNG reads exactly its eight photometries; TIFF 6.0's others are refused.
    #[test]
    fn dng_photometric_reads_only_dngs_codes() {
        for (code, p) in [
            (1, PhotometricInterpretation::BlackIsZero),
            (2, PhotometricInterpretation::Rgb),
            (4, PhotometricInterpretation::TransparencyMask),
            (6, PhotometricInterpretation::YCbCr),
            (32803, PhotometricInterpretation::Cfa),
            (34892, PhotometricInterpretation::LinearRaw),
            (51177, PhotometricInterpretation::Depth),
            (52527, PhotometricInterpretation::PhotometricMask),
        ] {
            assert_eq!(dng_photometric(code), Some(p), "{code}");
        }
        // WhiteIsZero, Palette, CMYK, the unassigned 7, and CIE L*a*b* are not DNG photometries.
        for bad in [0, 3, 5, 7, 8] {
            assert_eq!(dng_photometric(bad), None, "{bad}");
        }
    }

    #[test]
    fn illuminant_codes_round_trip() {
        // Spot-check the boundary and skipped codes plus the spectral-data sentinel.
        for code in [0u16, 1, 4, 9, 17, 21, 23, 24, 255] {
            let i = CalibrationIlluminant::try_from(code).expect("known illuminant");
            assert_eq!(u16::from(i), code);
        }
        assert_eq!(CalibrationIlluminant::try_from(5).ok(), None); // 5..=8 are unused
        assert_eq!(u16::from(CalibrationIlluminant::D65), 21);
        assert_eq!(u16::from(CalibrationIlluminant::D50), 23);
    }

    #[test]
    fn cfa_layout_codes_round_trip() {
        for code in 1..=9u16 {
            let l = CfaLayout::try_from(code).expect("known layout");
            assert_eq!(u16::from(l), code);
        }
        assert_eq!(CfaLayout::try_from(0).ok(), None);
        assert_eq!(CfaLayout::try_from(10).ok(), None);
        assert_eq!(CfaLayout::default(), CfaLayout::Rectangular);
    }

    #[test]
    fn other_enums_round_trip() {
        for p in [
            ProfileEmbedPolicy::AllowCopying,
            ProfileEmbedPolicy::EmbedIfUsed,
            ProfileEmbedPolicy::EmbedNever,
            ProfileEmbedPolicy::NoRestrictions,
        ] {
            assert_eq!(ProfileEmbedPolicy::try_from(u32::from(p)).ok(), Some(p));
        }
        assert_eq!(ProfileEmbedPolicy::try_from(4).ok(), None);

        for c in [
            PreviewColorSpace::Unknown,
            PreviewColorSpace::GrayGamma22,
            PreviewColorSpace::Srgb,
            PreviewColorSpace::AdobeRgb,
            PreviewColorSpace::ProPhotoRgb,
        ] {
            assert_eq!(PreviewColorSpace::try_from(u32::from(c)).ok(), Some(c));
        }
        assert_eq!(PreviewColorSpace::try_from(5).ok(), None);

        for e in [TableEncoding::Linear, TableEncoding::Srgb] {
            assert_eq!(TableEncoding::try_from(u32::from(e)).ok(), Some(e));
        }
        assert_eq!(TableEncoding::default(), TableEncoding::Linear);
        assert_eq!(TableEncoding::try_from(2).ok(), None);
    }

    #[test]
    fn calibration_illuminant_round_trips_every_variant() {
        for illum in [
            CalibrationIlluminant::Unknown,
            CalibrationIlluminant::Daylight,
            CalibrationIlluminant::Fluorescent,
            CalibrationIlluminant::Tungsten,
            CalibrationIlluminant::Flash,
            CalibrationIlluminant::FineWeather,
            CalibrationIlluminant::CloudyWeather,
            CalibrationIlluminant::Shade,
            CalibrationIlluminant::DaylightFluorescent,
            CalibrationIlluminant::DayWhiteFluorescent,
            CalibrationIlluminant::CoolWhiteFluorescent,
            CalibrationIlluminant::WhiteFluorescent,
            CalibrationIlluminant::WarmWhiteFluorescent,
            CalibrationIlluminant::StandardLightA,
            CalibrationIlluminant::StandardLightB,
            CalibrationIlluminant::StandardLightC,
            CalibrationIlluminant::D55,
            CalibrationIlluminant::D65,
            CalibrationIlluminant::D75,
            CalibrationIlluminant::D50,
            CalibrationIlluminant::IsoStudioTungsten,
            CalibrationIlluminant::Other,
        ] {
            assert_eq!(
                CalibrationIlluminant::try_from(u16::from(illum)).ok(),
                Some(illum),
                "{illum:?} (code {})",
                u16::from(illum)
            );
        }
        // Codes between the named ones (e.g. 5..=8) and out-of-range values are unrecognised.
        assert_eq!(CalibrationIlluminant::try_from(5).ok(), None);
        assert_eq!(CalibrationIlluminant::try_from(100).ok(), None);
    }
}
