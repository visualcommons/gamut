//! The value codes of the four tags every TIFF-based codec shares: `Compression` (259),
//! `PhotometricInterpretation` (262), `Predictor` (317) and `SampleFormat` (339).
//!
//! Each enum is the **union** of the codes the vendored specifications assign — TIFF 6.0
//! (`references/tiff/tiff6.pdf`, Section 8 and Part 2) with its Technical Notes, and DNG 1.7.1.0
//! (`references/dng/DNG_Spec_1_7_1_0.pdf`, Chapter 4) — so gamut-tiff and gamut-dng name a code
//! the same way. Each is `#[repr(u16)]` with the on-disk code as its discriminant, converts back
//! with `u16::from`, and parses with one `TryFrom<u16>` that refuses a code no specification
//! here assigns.
//!
//! Recognising a code is not supporting it. Which schemes a codec decodes or encodes, and which
//! codes its own profile admits at all, stays with the codec: gamut-tiff refuses the DNG-only
//! codes and gamut-dng the TIFF-only ones, each by variant.

/// The `unsupported` error every `TryFrom<u16>` below returns for an unassigned code.
fn unassigned(message: &'static str) -> gamut_core::Error {
    gamut_core::Error::unsupported(env!("CARGO_PKG_NAME"), message)
}

/// The compression scheme of an image's strips or tiles, stored in the `Compression` tag (259).
///
/// `#[non_exhaustive]`: TIFF registers further schemes, and later specifications keep adding them,
/// so recognised codes may be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u16)]
#[non_exhaustive]
pub enum Compression {
    /// `1` — no compression; samples are packed into bytes as tightly as possible (TIFF 6.0 §3).
    #[default]
    None = 1,
    /// `2` — CCITT Group 3 1-Dimensional Modified Huffman run-length encoding (TIFF 6.0 §10).
    CcittRle = 2,
    /// `3` — CCITT T.4 (Group 3) bilevel fax encoding (TIFF 6.0 §11).
    CcittGroup3Fax = 3,
    /// `4` — CCITT T.6 (Group 4) bilevel fax encoding (TIFF 6.0 §11).
    CcittGroup4Fax = 4,
    /// `5` — LZW (TIFF 6.0 §13).
    Lzw = 5,
    /// `6` — the deprecated old-style JPEG process (TIFF 6.0 §22).
    OldJpeg = 6,
    /// `7` — JPEG, the redefined "new-style" process (TIFF Technical Note 2). DNG stores its
    /// lossless-JPEG raw data under this code.
    Jpeg = 7,
    /// `8` — Deflate in the zlib data format (Adobe TIFF Technical Note 3). The legacy Adobe code
    /// `32946` reads as this variant too; it is never written.
    Deflate = 8,
    /// `32773` — PackBits, a byte-oriented run-length scheme (TIFF 6.0 §9).
    PackBits = 32773,
    /// `34892` — DNG lossy JPEG (DNG 1.4).
    LossyJpeg = 34892,
    /// `52546` — JPEG XL (DNG 1.7).
    JpegXl = 52546,
}

impl TryFrom<u16> for Compression {
    type Error = gamut_core::Error;

    /// Maps an on-disk `Compression` code to its scheme. The legacy Adobe Deflate code `32946`
    /// maps to [`Compression::Deflate`], like the standardised `8`.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code no
    /// vendored specification assigns.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => Compression::None,
            2 => Compression::CcittRle,
            3 => Compression::CcittGroup3Fax,
            4 => Compression::CcittGroup4Fax,
            5 => Compression::Lzw,
            6 => Compression::OldJpeg,
            7 => Compression::Jpeg,
            8 | 32946 => Compression::Deflate,
            32773 => Compression::PackBits,
            34892 => Compression::LossyJpeg,
            52546 => Compression::JpegXl,
            _ => return Err(unassigned("unrecognised Compression code")),
        })
    }
}

impl From<Compression> for u16 {
    /// The on-disk code, the `SHORT` written to tag 259.
    fn from(compression: Compression) -> Self {
        compression as u16
    }
}

/// How samples map to colour or raw photometry, stored in the `PhotometricInterpretation` tag
/// (262).
///
/// `#[non_exhaustive]`: TIFF extensions define further interpretations (LogL, LogLuv, …), so
/// variants may be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
#[non_exhaustive]
pub enum PhotometricInterpretation {
    /// `0` — bilevel/grayscale where `0` is white and the maximum value is black.
    WhiteIsZero = 0,
    /// `1` — bilevel/grayscale where `0` is black and the maximum value is white.
    BlackIsZero = 1,
    /// `2` — full-colour RGB.
    Rgb = 2,
    /// `3` — palette colour: sample values index a `ColorMap`.
    Palette = 3,
    /// `4` — a transparency mask (an auxiliary bilevel image for another image).
    TransparencyMask = 4,
    /// `5` — CMYK separated colour (TIFF 6.0 §16).
    Cmyk = 5,
    /// `6` — YCbCr colour (TIFF 6.0 §21); in DNG, the colour space of embedded JPEG previews.
    YCbCr = 6,
    /// `8` — CIE L\*a\*b\* colour (TIFF 6.0 §23).
    CieLab = 8,
    /// `32803` — a colour-filter-array (mosaic) raw image (TIFF/EP, DNG).
    Cfa = 32803,
    /// `34892` — a demosaiced ("linear") raw image, one sample per plane per pixel (DNG).
    LinearRaw = 34892,
    /// `51177` — a depth map (DNG 1.5).
    Depth = 51177,
    /// `52527` — a photometric (per-channel) mask (DNG 1.6).
    PhotometricMask = 52527,
}

impl TryFrom<u16> for PhotometricInterpretation {
    type Error = gamut_core::Error;

    /// Maps an on-disk `PhotometricInterpretation` code to its interpretation.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code no
    /// vendored specification assigns — `7`, between `YCbCr = 6` and `CieLab = 8`, among them.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => PhotometricInterpretation::WhiteIsZero,
            1 => PhotometricInterpretation::BlackIsZero,
            2 => PhotometricInterpretation::Rgb,
            3 => PhotometricInterpretation::Palette,
            4 => PhotometricInterpretation::TransparencyMask,
            5 => PhotometricInterpretation::Cmyk,
            6 => PhotometricInterpretation::YCbCr,
            8 => PhotometricInterpretation::CieLab,
            32803 => PhotometricInterpretation::Cfa,
            34892 => PhotometricInterpretation::LinearRaw,
            51177 => PhotometricInterpretation::Depth,
            52527 => PhotometricInterpretation::PhotometricMask,
            _ => return Err(unassigned("unrecognised PhotometricInterpretation code")),
        })
    }
}

impl From<PhotometricInterpretation> for u16 {
    /// The on-disk code, the `SHORT` written to tag 262.
    fn from(photometric: PhotometricInterpretation) -> Self {
        photometric as u16
    }
}

/// The prediction scheme applied before compression, stored in the `Predictor` tag (317,
/// TIFF 6.0 §14).
///
/// `#[non_exhaustive]`: further schemes may be registered, so variants may be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u16)]
#[non_exhaustive]
pub enum Predictor {
    /// `1` — no prediction.
    #[default]
    None = 1,
    /// `2` — horizontal differencing: each sample is stored as its difference from the sample to
    /// its left (TIFF 6.0 §14).
    HorizontalDifferencing = 2,
    /// `3` — floating-point horizontal differencing (Adobe Photoshop TIFF Technical Note 3).
    FloatingPoint = 3,
    /// `34892` — horizontal differencing against the pixel two to the left (DNG).
    HorizontalDifferencingX2 = 34892,
    /// `34893` — horizontal differencing against the pixel four to the left (DNG).
    HorizontalDifferencingX4 = 34893,
    /// `34894` — floating-point differencing against the pixel two to the left (DNG).
    FloatingPointX2 = 34894,
    /// `34895` — floating-point differencing against the pixel four to the left (DNG).
    FloatingPointX4 = 34895,
}

impl TryFrom<u16> for Predictor {
    type Error = gamut_core::Error;

    /// Maps an on-disk `Predictor` code to its scheme.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code no
    /// vendored specification assigns.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => Predictor::None,
            2 => Predictor::HorizontalDifferencing,
            3 => Predictor::FloatingPoint,
            34892 => Predictor::HorizontalDifferencingX2,
            34893 => Predictor::HorizontalDifferencingX4,
            34894 => Predictor::FloatingPointX2,
            34895 => Predictor::FloatingPointX4,
            _ => return Err(unassigned("unrecognised Predictor code")),
        })
    }
}

impl From<Predictor> for u16 {
    /// The on-disk code, the `SHORT` written to tag 317.
    fn from(predictor: Predictor) -> Self {
        predictor as u16
    }
}

/// How a sample's bits are interpreted, stored in the `SampleFormat` tag (339, TIFF 6.0 §19).
///
/// When the tag is absent the format is unsigned integer, the TIFF 6.0 default.
///
/// `#[non_exhaustive]`: further codes may be registered, so variants may be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u16)]
#[non_exhaustive]
pub enum SampleFormat {
    /// `1` — unsigned integer (the default when the tag is absent).
    #[default]
    UnsignedInteger = 1,
    /// `2` — two's-complement signed integer.
    SignedInteger = 2,
    /// `3` — IEEE floating point.
    FloatingPoint = 3,
    /// `4` — undefined data format.
    Undefined = 4,
}

impl TryFrom<u16> for SampleFormat {
    type Error = gamut_core::Error;

    /// Maps an on-disk `SampleFormat` code to its interpretation.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code no
    /// vendored specification assigns. It never falls back to unsigned integer: an IEEE-float or
    /// two's-complement page read as unsigned decodes to plausible nonsense rather than failing.
    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => SampleFormat::UnsignedInteger,
            2 => SampleFormat::SignedInteger,
            3 => SampleFormat::FloatingPoint,
            4 => SampleFormat::Undefined,
            _ => return Err(unassigned("unrecognised SampleFormat code")),
        })
    }
}

impl From<SampleFormat> for u16 {
    /// The on-disk code, the `SHORT` written to tag 339.
    fn from(format: SampleFormat) -> Self {
        format as u16
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    /// Each `assigned` value writes its code and reads back from it; each `aliases` code reads as
    /// its value without being written; every other code in the `u16` space is refused as
    /// unsupported. The sweep is exhaustive, so a dropped, extra or misnumbered arm in either
    /// direction fails it.
    fn assert_codes<T>(assigned: &[(T, u16)], aliases: &[(T, u16)])
    where
        T: TryFrom<u16, Error = gamut_core::Error>
            + Into<u16>
            + Copy
            + PartialEq
            + core::fmt::Debug,
    {
        for &(value, code) in assigned {
            assert_eq!(value.into(), code, "{value:?}");
        }
        for code in 0..=u16::MAX {
            match assigned.iter().chain(aliases).find(|&&(_, c)| c == code) {
                Some(&(value, _)) => assert_eq!(T::try_from(code).unwrap(), value, "{code}"),
                None => assert_eq!(
                    T::try_from(code).map(|_| ()).unwrap_err().kind(),
                    ErrorKind::Unsupported,
                    "{code}"
                ),
            }
        }
    }

    #[test]
    fn compression_codes_match_the_specs() {
        assert_eq!(Compression::default(), Compression::None);
        assert_codes(
            &[
                (Compression::None, 1),
                (Compression::CcittRle, 2),
                (Compression::CcittGroup3Fax, 3),
                (Compression::CcittGroup4Fax, 4),
                (Compression::Lzw, 5),
                (Compression::OldJpeg, 6),
                (Compression::Jpeg, 7),
                (Compression::Deflate, 8),
                (Compression::PackBits, 32773),
                (Compression::LossyJpeg, 34892),
                (Compression::JpegXl, 52546),
            ],
            // The legacy Adobe Deflate code reads but is never written.
            &[(Compression::Deflate, 32946)],
        );
    }

    #[test]
    fn photometric_codes_match_the_specs() {
        assert_codes(
            &[
                (PhotometricInterpretation::WhiteIsZero, 0),
                (PhotometricInterpretation::BlackIsZero, 1),
                (PhotometricInterpretation::Rgb, 2),
                (PhotometricInterpretation::Palette, 3),
                (PhotometricInterpretation::TransparencyMask, 4),
                (PhotometricInterpretation::Cmyk, 5),
                (PhotometricInterpretation::YCbCr, 6),
                (PhotometricInterpretation::CieLab, 8),
                (PhotometricInterpretation::Cfa, 32803),
                (PhotometricInterpretation::LinearRaw, 34892),
                (PhotometricInterpretation::Depth, 51177),
                (PhotometricInterpretation::PhotometricMask, 52527),
            ],
            &[],
        );
    }

    #[test]
    fn predictor_codes_match_the_specs() {
        assert_eq!(Predictor::default(), Predictor::None);
        assert_codes(
            &[
                (Predictor::None, 1),
                (Predictor::HorizontalDifferencing, 2),
                (Predictor::FloatingPoint, 3),
                (Predictor::HorizontalDifferencingX2, 34892),
                (Predictor::HorizontalDifferencingX4, 34893),
                (Predictor::FloatingPointX2, 34894),
                (Predictor::FloatingPointX4, 34895),
            ],
            &[],
        );
    }

    #[test]
    fn sample_format_codes_match_the_specs() {
        assert_eq!(SampleFormat::default(), SampleFormat::UnsignedInteger);
        assert_codes(
            &[
                (SampleFormat::UnsignedInteger, 1),
                (SampleFormat::SignedInteger, 2),
                (SampleFormat::FloatingPoint, 3),
                (SampleFormat::Undefined, 4),
            ],
            &[],
        );
    }
}
