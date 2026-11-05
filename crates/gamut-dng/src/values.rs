//! Enumerated DNG tag *values* — the codes stored in the `CalibrationIlluminant`, `CFALayout`,
//! `ProfileEmbedPolicy`, `PreviewColorSpace`, and `TableEncoding` tags, plus the
//! `NewSubFileType` bit values.
//!
//! Values come from the **DNG 1.7.1.0** specification and the Adobe DNG SDK's `dng_tag_values.h`.
//! Each enum mirrors the structural-codec pattern used elsewhere in the workspace: a `from_code`
//! that rejects unknown codes (`None`) and a `code` that returns the on-disk value.
//!
//! The `Compression`, `PhotometricInterpretation`, `Predictor` and `SampleFormat` codes are
//! gamut-ifd's, shared with gamut-tiff and re-exported here: each is a `#[repr(u16)]` enum read
//! with `TryFrom<u16>` and written with `u16::from`. Their union carries codes only TIFF assigns
//! (CCITT, LZW and PackBits compression, the palette and CMYK photometries, …), and what stays
//! here is which of them a DNG reader admits. What gamut-dng does with a `Compression` is asked
//! through the free functions [`is_decodable`] and [`is_lossy`], which replace the inherent
//! methods of the same names the crate-local enum carried.

pub use gamut_ifd::tags::{Compression, PhotometricInterpretation, Predictor, SampleFormat};

pub use crate::compression::{is_decodable, is_lossy};

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

/// A calibration illuminant, stored in `CalibrationIlluminant1/2/3` (and equal to the EXIF
/// `LightSource` codes).
///
/// [`Other`](Self::Other) (`255`) signals that spectral data is supplied separately via the
/// `IlluminantData1/2/3` tags instead of a named illuminant.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationIlluminant {
    /// `0` — unknown.
    Unknown,
    /// `1` — daylight.
    Daylight,
    /// `2` — fluorescent.
    Fluorescent,
    /// `3` — tungsten (incandescent).
    Tungsten,
    /// `4` — flash.
    Flash,
    /// `9` — fine weather.
    FineWeather,
    /// `10` — cloudy weather.
    CloudyWeather,
    /// `11` — shade.
    Shade,
    /// `12` — daylight fluorescent (D, 5700–7100 K).
    DaylightFluorescent,
    /// `13` — day-white fluorescent (N, 4600–5500 K).
    DayWhiteFluorescent,
    /// `14` — cool-white fluorescent (W, 3800–4500 K).
    CoolWhiteFluorescent,
    /// `15` — white fluorescent (WW, 3250–3800 K).
    WhiteFluorescent,
    /// `16` — warm-white fluorescent (L, 2600–3250 K).
    WarmWhiteFluorescent,
    /// `17` — Standard Light A.
    StandardLightA,
    /// `18` — Standard Light B.
    StandardLightB,
    /// `19` — Standard Light C.
    StandardLightC,
    /// `20` — D55.
    D55,
    /// `21` — D65.
    D65,
    /// `22` — D75.
    D75,
    /// `23` — D50.
    D50,
    /// `24` — ISO studio tungsten.
    IsoStudioTungsten,
    /// `255` — other (spectral data given via `IlluminantData`).
    Other,
}

impl CalibrationIlluminant {
    /// Returns the illuminant for an on-disk tag value, or `None` if unrecognised.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
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
            _ => return None,
        })
    }

    /// Returns the on-disk tag value.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            CalibrationIlluminant::Unknown => 0,
            CalibrationIlluminant::Daylight => 1,
            CalibrationIlluminant::Fluorescent => 2,
            CalibrationIlluminant::Tungsten => 3,
            CalibrationIlluminant::Flash => 4,
            CalibrationIlluminant::FineWeather => 9,
            CalibrationIlluminant::CloudyWeather => 10,
            CalibrationIlluminant::Shade => 11,
            CalibrationIlluminant::DaylightFluorescent => 12,
            CalibrationIlluminant::DayWhiteFluorescent => 13,
            CalibrationIlluminant::CoolWhiteFluorescent => 14,
            CalibrationIlluminant::WhiteFluorescent => 15,
            CalibrationIlluminant::WarmWhiteFluorescent => 16,
            CalibrationIlluminant::StandardLightA => 17,
            CalibrationIlluminant::StandardLightB => 18,
            CalibrationIlluminant::StandardLightC => 19,
            CalibrationIlluminant::D55 => 20,
            CalibrationIlluminant::D65 => 21,
            CalibrationIlluminant::D75 => 22,
            CalibrationIlluminant::D50 => 23,
            CalibrationIlluminant::IsoStudioTungsten => 24,
            CalibrationIlluminant::Other => 255,
        }
    }
}

/// The physical layout of the colour filter array, stored in `CFALayout` (50711).
///
/// `1` is the common rectangular (square) grid (e.g. ordinary Bayer); `2`–`9` are the staggered
/// layouts where alternate rows or columns are offset by half a pixel.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CfaLayout {
    /// `1` — rectangular (square) layout.
    #[default]
    Rectangular,
    /// `2` — staggered layout A (even columns shifted down by 1/2 row).
    StaggeredA,
    /// `3` — staggered layout B (even columns shifted up by 1/2 row).
    StaggeredB,
    /// `4` — staggered layout C (even rows shifted right by 1/2 column).
    StaggeredC,
    /// `5` — staggered layout D (even rows shifted left by 1/2 column).
    StaggeredD,
    /// `6` — staggered layout E.
    StaggeredE,
    /// `7` — staggered layout F.
    StaggeredF,
    /// `8` — staggered layout G.
    StaggeredG,
    /// `9` — staggered layout H.
    StaggeredH,
}

impl CfaLayout {
    /// Returns the layout for an on-disk tag value, or `None` if out of range.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => CfaLayout::Rectangular,
            2 => CfaLayout::StaggeredA,
            3 => CfaLayout::StaggeredB,
            4 => CfaLayout::StaggeredC,
            5 => CfaLayout::StaggeredD,
            6 => CfaLayout::StaggeredE,
            7 => CfaLayout::StaggeredF,
            8 => CfaLayout::StaggeredG,
            9 => CfaLayout::StaggeredH,
            _ => return None,
        })
    }

    /// Returns the on-disk tag value.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            CfaLayout::Rectangular => 1,
            CfaLayout::StaggeredA => 2,
            CfaLayout::StaggeredB => 3,
            CfaLayout::StaggeredC => 4,
            CfaLayout::StaggeredD => 5,
            CfaLayout::StaggeredE => 6,
            CfaLayout::StaggeredF => 7,
            CfaLayout::StaggeredG => 8,
            CfaLayout::StaggeredH => 9,
        }
    }
}

/// The embedding/usage policy of a camera profile, stored in `ProfileEmbedPolicy` (50941).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileEmbedPolicy {
    /// `0` — the profile may be freely copied.
    #[default]
    AllowCopying,
    /// `1` — embed the profile only in files it was used to render.
    EmbedIfUsed,
    /// `2` — never embed the profile.
    EmbedNever,
    /// `3` — no restrictions.
    NoRestrictions,
}

impl ProfileEmbedPolicy {
    /// Returns the policy for an on-disk tag value, or `None` if unrecognised.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => ProfileEmbedPolicy::AllowCopying,
            1 => ProfileEmbedPolicy::EmbedIfUsed,
            2 => ProfileEmbedPolicy::EmbedNever,
            3 => ProfileEmbedPolicy::NoRestrictions,
            _ => return None,
        })
    }

    /// Returns the on-disk tag value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            ProfileEmbedPolicy::AllowCopying => 0,
            ProfileEmbedPolicy::EmbedIfUsed => 1,
            ProfileEmbedPolicy::EmbedNever => 2,
            ProfileEmbedPolicy::NoRestrictions => 3,
        }
    }
}

/// The colour space of a preview image, stored in `PreviewColorSpace` (50970).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewColorSpace {
    /// `0` — unknown.
    #[default]
    Unknown,
    /// `1` — grayscale, gamma 2.2.
    GrayGamma22,
    /// `2` — sRGB.
    Srgb,
    /// `3` — Adobe RGB (1998).
    AdobeRgb,
    /// `4` — ProPhoto RGB.
    ProPhotoRgb,
}

impl PreviewColorSpace {
    /// Returns the colour space for an on-disk tag value, or `None` if unrecognised.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => PreviewColorSpace::Unknown,
            1 => PreviewColorSpace::GrayGamma22,
            2 => PreviewColorSpace::Srgb,
            3 => PreviewColorSpace::AdobeRgb,
            4 => PreviewColorSpace::ProPhotoRgb,
            _ => return None,
        })
    }

    /// Returns the on-disk tag value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            PreviewColorSpace::Unknown => 0,
            PreviewColorSpace::GrayGamma22 => 1,
            PreviewColorSpace::Srgb => 2,
            PreviewColorSpace::AdobeRgb => 3,
            PreviewColorSpace::ProPhotoRgb => 4,
        }
    }
}

/// How a 3D hue/saturation/value table is indexed, stored in `ProfileHueSatMapEncoding` (51107)
/// and `ProfileLookTableEncoding` (51108).
///
/// Both tags default to [`Linear`](Self::Linear) when absent, and neither applies to a 2.5D table
/// (one whose value division count is 1).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableEncoding {
    /// `0` — the table is indexed by linear ProPhoto RGB converted straight to HSV.
    #[default]
    Linear,
    /// `1` — the value coordinate is sRGB-encoded before indexing, giving shadows more table
    /// precision.
    Srgb,
}

impl TableEncoding {
    /// Returns the encoding for an on-disk tag value, or `None` if unrecognised.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => TableEncoding::Linear,
            1 => TableEncoding::Srgb,
            _ => return None,
        })
    }

    /// Returns the on-disk tag value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            TableEncoding::Linear => 0,
            TableEncoding::Srgb => 1,
        }
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
            let i = CalibrationIlluminant::from_code(code).expect("known illuminant");
            assert_eq!(i.code(), code);
        }
        assert_eq!(CalibrationIlluminant::from_code(5), None); // 5..=8 are unused
        assert_eq!(CalibrationIlluminant::D65.code(), 21);
        assert_eq!(CalibrationIlluminant::D50.code(), 23);
    }

    #[test]
    fn cfa_layout_codes_round_trip() {
        for code in 1..=9u16 {
            let l = CfaLayout::from_code(code).expect("known layout");
            assert_eq!(l.code(), code);
        }
        assert_eq!(CfaLayout::from_code(0), None);
        assert_eq!(CfaLayout::from_code(10), None);
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
            assert_eq!(ProfileEmbedPolicy::from_code(p.code()), Some(p));
        }
        assert_eq!(ProfileEmbedPolicy::from_code(4), None);

        for c in [
            PreviewColorSpace::Unknown,
            PreviewColorSpace::GrayGamma22,
            PreviewColorSpace::Srgb,
            PreviewColorSpace::AdobeRgb,
            PreviewColorSpace::ProPhotoRgb,
        ] {
            assert_eq!(PreviewColorSpace::from_code(c.code()), Some(c));
        }
        assert_eq!(PreviewColorSpace::from_code(5), None);

        for e in [TableEncoding::Linear, TableEncoding::Srgb] {
            assert_eq!(TableEncoding::from_code(e.code()), Some(e));
        }
        assert_eq!(TableEncoding::default(), TableEncoding::Linear);
        assert_eq!(TableEncoding::from_code(2), None);
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
                CalibrationIlluminant::from_code(illum.code()),
                Some(illum),
                "{illum:?} (code {})",
                illum.code()
            );
        }
        // Codes between the named ones (e.g. 5..=8) and out-of-range values are unrecognised.
        assert_eq!(CalibrationIlluminant::from_code(5), None);
        assert_eq!(CalibrationIlluminant::from_code(100), None);
    }
}
