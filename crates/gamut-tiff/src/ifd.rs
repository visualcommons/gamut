//! Codec-specific interpretations of two IFD tag values: the photometric interpretation and the
//! prediction scheme.
//!
//! The structural IFD model — the byte-order header, [`gamut_ifd::FieldType`] / [`gamut_ifd::Value`],
//! the [`gamut_ifd::Ifd`] chain, and the read/write spine — lives in the shared
//! [`gamut_ifd`](https://crates.io/crates/gamut-ifd) crate (re-exported from this crate's root).
//! What stays here is *codec semantics*: how a [`PhotometricInterpretation`] maps samples to colour
//! and which [`Predictor`] is applied before compression. These are the meanings the encoder and
//! decoder attach to the `PhotometricInterpretation` (262) and `Predictor` (317) tags, not part of
//! the container structure.

/// How pixel samples map to colour, stored in the `PhotometricInterpretation` tag (262).
///
/// The set is non-exhaustive: TIFF extensions define further interpretations (LogL, LogLuv,
/// the TIFF/EP CFA and LinearRaw values, …), so variants may be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PhotometricInterpretation {
    /// `0` — bilevel/grayscale where `0` is white and the maximum value is black.
    WhiteIsZero,
    /// `1` — bilevel/grayscale where `0` is black and the maximum value is white.
    BlackIsZero,
    /// `2` — full-colour RGB.
    Rgb,
    /// `3` — palette colour: sample values index a `ColorMap`.
    Palette,
    /// `4` — a transparency mask (an auxiliary bilevel image for another image).
    TransparencyMask,
    /// `5` — CMYK separated colour (TIFF 6.0 §16).
    Cmyk,
    /// `6` — YCbCr colour (TIFF 6.0 §21).
    YCbCr,
    /// `8` — CIE L\*a\*b\* colour (TIFF 6.0 §23).
    CieLab,
}

impl TryFrom<u32> for PhotometricInterpretation {
    type Error = gamut_core::Error;

    /// Maps an on-disk `PhotometricInterpretation` tag value (tag 262) to its interpretation.
    ///
    /// Code `7` is unassigned in TIFF 6.0 (it sits between `YCbCr = 6` and `CieLab = 8`);
    /// it and every other unrecognised code fail with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            0 => PhotometricInterpretation::WhiteIsZero,
            1 => PhotometricInterpretation::BlackIsZero,
            2 => PhotometricInterpretation::Rgb,
            3 => PhotometricInterpretation::Palette,
            4 => PhotometricInterpretation::TransparencyMask,
            5 => PhotometricInterpretation::Cmyk,
            6 => PhotometricInterpretation::YCbCr,
            8 => PhotometricInterpretation::CieLab,
            _ => {
                return Err(gamut_core::Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unrecognised PhotometricInterpretation tag value",
                ));
            }
        })
    }
}

impl From<PhotometricInterpretation> for u16 {
    /// Returns the on-disk tag value (the `SHORT` written to tag 262).
    fn from(photometric: PhotometricInterpretation) -> Self {
        match photometric {
            PhotometricInterpretation::WhiteIsZero => 0,
            PhotometricInterpretation::BlackIsZero => 1,
            PhotometricInterpretation::Rgb => 2,
            PhotometricInterpretation::Palette => 3,
            PhotometricInterpretation::TransparencyMask => 4,
            PhotometricInterpretation::Cmyk => 5,
            PhotometricInterpretation::YCbCr => 6,
            PhotometricInterpretation::CieLab => 8,
        }
    }
}

/// How a sample's bits are interpreted, stored in the `SampleFormat` tag (339, TIFF 6.0 §19).
///
/// When the tag is absent the format is unsigned integer — the TIFF 6.0 default, and the only
/// encoding this crate's `u8`/`u16` sample model represents. The others are recognised so the
/// decoder can name what it is refusing: an IEEE-float or two's-complement page read as unsigned
/// would decode to plausible nonsense rather than fail.
///
/// The set is non-exhaustive: the TIFF Technical Notes may register further codes, so variants may
/// be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SampleFormat {
    /// `1` — unsigned integer (the default when the tag is absent).
    #[default]
    UnsignedInteger,
    /// `2` — two's-complement signed integer.
    SignedInteger,
    /// `3` — IEEE floating point.
    FloatingPoint,
    /// `4` — undefined data format.
    Undefined,
}

impl TryFrom<u32> for SampleFormat {
    type Error = gamut_core::Error;

    /// Maps an on-disk `SampleFormat` tag value (tag 339) to its interpretation.
    ///
    /// Unrecognised codes fail with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error rather than falling back to
    /// unsigned integer: guessing here is exactly the silent misdecode the typed error exists to
    /// prevent.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => SampleFormat::UnsignedInteger,
            2 => SampleFormat::SignedInteger,
            3 => SampleFormat::FloatingPoint,
            4 => SampleFormat::Undefined,
            _ => {
                return Err(gamut_core::Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unrecognised SampleFormat tag value",
                ));
            }
        })
    }
}

impl From<SampleFormat> for u16 {
    /// Returns the on-disk tag value (the `SHORT` written to tag 339).
    fn from(format: SampleFormat) -> Self {
        match format {
            SampleFormat::UnsignedInteger => 1,
            SampleFormat::SignedInteger => 2,
            SampleFormat::FloatingPoint => 3,
            SampleFormat::Undefined => 4,
        }
    }
}

/// The prediction scheme applied before compression, stored in the `Predictor` tag (317,
/// TIFF 6.0 §14).
///
/// [`Predictor::HorizontalDifferencing`] covers 8- and 16-bit samples; it differences sample
/// *values*, so the two widths are distinct operations rather than one loop over a wider stride.
///
/// The set is non-exhaustive: the TIFF Technical Notes define predictor `3` (floating-point
/// horizontal differencing, deferred along with float samples themselves), so variants may be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Predictor {
    /// `1` — no prediction.
    #[default]
    None,
    /// `2` — horizontal differencing: each sample is stored as its difference from the sample to
    /// its left.
    HorizontalDifferencing,
}

impl TryFrom<u32> for Predictor {
    type Error = gamut_core::Error;

    /// Maps an on-disk `Predictor` tag value (tag 317) to its scheme.
    ///
    /// The TIFF Technical Notes also define `3` (floating-point horizontal differencing); it is
    /// deferred along with float samples themselves (see `STATUS.md`) and, like every other
    /// unrecognised code, fails with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => Predictor::None,
            2 => Predictor::HorizontalDifferencing,
            _ => {
                return Err(gamut_core::Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unrecognised Predictor tag value",
                ));
            }
        })
    }
}

impl From<Predictor> for u16 {
    /// Returns the on-disk tag value (the `SHORT` written to tag 317).
    fn from(predictor: Predictor) -> Self {
        match predictor {
            Predictor::None => 1,
            Predictor::HorizontalDifferencing => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photometric_codes_round_trip() {
        // Every recognised PhotometricInterpretation code maps back to itself; note `7` is
        // unassigned (skipped between YCbCr=6 and CieLab=8) and unknown codes are rejected.
        for code in [0u32, 1, 2, 3, 4, 5, 6, 8] {
            let p = PhotometricInterpretation::try_from(code).expect("known photometric");
            assert_eq!(u32::from(u16::from(p)), code);
        }
        assert!(PhotometricInterpretation::try_from(7).is_err());
        assert!(PhotometricInterpretation::try_from(9).is_err());
    }

    #[test]
    fn sample_format_codes_round_trip() {
        assert_eq!(SampleFormat::default(), SampleFormat::UnsignedInteger);
        for (format, code) in [
            (SampleFormat::UnsignedInteger, 1u16),
            (SampleFormat::SignedInteger, 2),
            (SampleFormat::FloatingPoint, 3),
            (SampleFormat::Undefined, 4),
        ] {
            assert_eq!(u16::from(format), code);
            assert_eq!(SampleFormat::try_from(u32::from(code)).unwrap(), format);
        }
        // `0` and codes past the registered set are refused rather than defaulted to unsigned.
        assert!(SampleFormat::try_from(0).is_err());
        assert!(SampleFormat::try_from(5).is_err());
    }

    #[test]
    fn predictor_codes_round_trip() {
        assert_eq!(Predictor::default(), Predictor::None);
        for (p, code) in [
            (Predictor::None, 1u16),
            (Predictor::HorizontalDifferencing, 2),
        ] {
            assert_eq!(u16::from(p), code);
            assert_eq!(Predictor::try_from(u32::from(code)).unwrap(), p);
        }
        // Predictor 3 (floating-point differencing) is deferred with float samples.
        assert!(Predictor::try_from(0).is_err());
        assert!(Predictor::try_from(3).is_err());
    }
}
