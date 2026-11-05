//! TIFF's reading of three shared tag-value codes: the photometric interpretation, the sample
//! format, and the prediction scheme.
//!
//! The codes themselves — [`PhotometricInterpretation`], [`SampleFormat`], [`Predictor`] — are
//! gamut-ifd's, shared with gamut-dng and re-exported from this crate's root. Their union carries
//! codes only DNG assigns (the CFA and LinearRaw photometries, the floating-point and `X2`/`X4`
//! predictors); what stays here is which of them a TIFF page may declare. Each reader refuses the
//! rest as unrecognised, so [`TiffInfo`](crate::TiffInfo) and [`deconstruct`](crate::deconstruct)
//! keep the verdicts they had when this crate kept its own copies of the enums.

use gamut_core::{Error, Result};
pub(crate) use gamut_ifd::tags::{PhotometricInterpretation, Predictor, SampleFormat};

/// Reads `code` as a `T` that `admits` accepts, or refuses it as unsupported with `message`.
///
/// A code past `u16` is refused like an unassigned one: every code these tags carry is a `SHORT`.
pub(crate) fn read<T: TryFrom<u16>>(
    code: u32,
    admits: fn(&T) -> bool,
    message: &'static str,
) -> Result<T> {
    u16::try_from(code)
        .ok()
        .and_then(|code| T::try_from(code).ok())
        .filter(admits)
        .ok_or_else(|| Error::unsupported(env!("CARGO_PKG_NAME"), message))
}

/// Reads an on-disk `PhotometricInterpretation` code (tag 262) as one TIFF 6.0 assigns.
///
/// Code `7` is unassigned (it sits between `YCbCr = 6` and `CieLab = 8`), and the TIFF/EP and DNG
/// raw photometries are not TIFF 6.0's.
///
/// # Errors
///
/// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for any other code.
pub(crate) fn tiff_photometric(code: u32) -> Result<PhotometricInterpretation> {
    read(
        code,
        |p| {
            matches!(
                p,
                PhotometricInterpretation::WhiteIsZero
                    | PhotometricInterpretation::BlackIsZero
                    | PhotometricInterpretation::Rgb
                    | PhotometricInterpretation::Palette
                    | PhotometricInterpretation::TransparencyMask
                    | PhotometricInterpretation::Cmyk
                    | PhotometricInterpretation::YCbCr
                    | PhotometricInterpretation::CieLab
            )
        },
        "TIFF: unrecognised PhotometricInterpretation tag value",
    )
}

/// Reads an on-disk `SampleFormat` code (tag 339, TIFF 6.0 §19).
///
/// Unrecognised codes fail rather than falling back to unsigned integer: guessing here is exactly
/// the silent misdecode the typed error exists to prevent.
///
/// # Errors
///
/// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code outside
/// §19's four.
pub(crate) fn tiff_sample_format(code: u32) -> Result<SampleFormat> {
    // Every code gamut-ifd assigns to this tag is §19's own, so TIFF narrows nothing.
    read(code, |_| true, "TIFF: unrecognised SampleFormat tag value")
}

/// Reads an on-disk `Predictor` code (tag 317, TIFF 6.0 §14).
///
/// Only `1` and `2` are read: the floating-point predictor `3` is deferred along with float
/// samples themselves (see `STATUS.md`), and the `X2`/`X4` predictors are DNG's.
///
/// # Errors
///
/// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for any other code.
pub(crate) fn tiff_predictor(code: u32) -> Result<Predictor> {
    read(
        code,
        |p| matches!(p, Predictor::None | Predictor::HorizontalDifferencing),
        "TIFF: unrecognised Predictor tag value",
    )
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    /// Asserts each `bad` code is refused as unsupported by `reader`.
    fn refuses<T: core::fmt::Debug>(reader: fn(u32) -> Result<T>, bad: &[u32]) {
        for &code in bad {
            assert_eq!(
                reader(code).unwrap_err().kind(),
                ErrorKind::Unsupported,
                "{code}"
            );
        }
    }

    #[test]
    fn tiff_photometric_reads_only_tiff6s_codes() {
        for (code, p) in [
            (0, PhotometricInterpretation::WhiteIsZero),
            (1, PhotometricInterpretation::BlackIsZero),
            (2, PhotometricInterpretation::Rgb),
            (3, PhotometricInterpretation::Palette),
            (4, PhotometricInterpretation::TransparencyMask),
            (5, PhotometricInterpretation::Cmyk),
            (6, PhotometricInterpretation::YCbCr),
            (8, PhotometricInterpretation::CieLab),
        ] {
            assert_eq!(tiff_photometric(code).unwrap(), p, "{code}");
        }
        // `7` is unassigned; the rest are the TIFF/EP and DNG raw photometries, and one past u16.
        refuses(
            tiff_photometric,
            &[7, 9, 32803, 34892, 51177, 52527, 65536 + 1],
        );
    }

    #[test]
    fn tiff_sample_format_reads_section_19s_codes() {
        for (code, f) in [
            (1, SampleFormat::UnsignedInteger),
            (2, SampleFormat::SignedInteger),
            (3, SampleFormat::FloatingPoint),
            (4, SampleFormat::Undefined),
        ] {
            assert_eq!(tiff_sample_format(code).unwrap(), f, "{code}");
        }
        // `0` and codes past the registered set are refused rather than defaulted to unsigned.
        refuses(tiff_sample_format, &[0, 5, 65536 + 1]);
    }

    #[test]
    fn tiff_predictor_reads_only_none_and_horizontal_differencing() {
        assert_eq!(tiff_predictor(1).unwrap(), Predictor::None);
        assert_eq!(
            tiff_predictor(2).unwrap(),
            Predictor::HorizontalDifferencing
        );
        // Predictor 3 (floating-point differencing) is deferred with float samples; the
        // 34892..=34895 predictors are DNG's.
        refuses(
            tiff_predictor,
            &[0, 3, 34892, 34893, 34894, 34895, 65536 + 2],
        );
    }
}
