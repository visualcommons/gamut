//! Undoing the `Predictor` tag (317) — the horizontal-differencing transform applied before
//! compression.
//!
//! DNG 1.7.1 p. 22 allows a non-null predictor only on IFDs using Deflate (8), and defines the
//! `X2`/`X4` variants as differencing against "the pixel two/four to the left" rather than one.
//! The reference SDK's reader is deliberately more lenient than the spec it validates: it also
//! honours the integer predictors on uncompressed data (`dng_read_image.cpp:2013-2074`, whose
//! comment cites writers that "preserve horizontal predictor data in otherwise uncompressed
//! TIFFs"), and ignores the tag entirely for JPEG and JPEG XL, whose codecs carry their own
//! internal prediction. This module matches the reader, because the job here is to read what
//! real files contain.
//!
//! The kernel mirrors `DecodeDelta8/16/32` (`dng_read_image.cpp:43-132`) exactly: rows are
//! independent (the recurrence restarts every row, seeded by that row's first `channels`
//! samples), the back-reference is `channels = samples_per_pixel * x_factor` elements, and the
//! addition **wraps at the container width** — no saturation and no mask to `BitsPerSample`.
//! Which is why a non-null predictor is confined to 8- and 16-bit data: those are the container
//! widths this crate's `u16` sample model can wrap at faithfully.

use gamut_core::{Error, Result};

use crate::values::{Compression, Predictor};

/// How many pixels to the left the predictor differences against, or `None` when this predictor
/// is not an integer horizontal differencer.
fn x_factor(predictor: Predictor) -> Option<usize> {
    match predictor {
        Predictor::None => Some(0),
        Predictor::HorizontalDifference => Some(1),
        Predictor::HorizontalDifferenceX2 => Some(2),
        Predictor::HorizontalDifferenceX4 => Some(4),
        _ => None,
    }
}

/// Validates the `Predictor` tag against the IFD's compression and bit depth, returning the
/// predictor to apply after each chunk is unpacked.
///
/// Returns [`Predictor::None`] for the schemes that carry their own prediction (lossless JPEG,
/// JPEG XL), matching the SDK reader, which never inspects the tag for them.
///
/// # Errors
///
/// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for the
/// floating-point predictors (3, 34894, 34895 — this crate rejects floating-point samples
/// outright) and for a non-null predictor at a bit depth whose container width the sample model
/// cannot wrap at, and an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error
/// for a predictor code outside the spec's set.
pub(crate) fn validate(
    code: Option<u32>,
    compression: Compression,
    bits: u16,
) -> Result<Predictor> {
    let Some(code) = code else {
        return Ok(Predictor::None);
    };
    let predictor = u16::try_from(code)
        .ok()
        .and_then(Predictor::from_code)
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: unknown Predictor code")
        })?;
    if predictor == Predictor::None {
        return Ok(Predictor::None);
    }
    // Lossless JPEG and JPEG XL predict internally; the SDK reader ignores tag 317 for them, so a
    // stray value must not change the pixels or fail the decode.
    if matches!(compression, Compression::LosslessJpeg | Compression::JpegXl) {
        return Ok(Predictor::None);
    }
    if x_factor(predictor).is_none() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: floating-point predictors are not supported",
        ));
    }
    if bits != 8 && bits != 16 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: a horizontal-difference predictor needs 8- or 16-bit samples",
        ));
    }
    Ok(predictor)
}

/// Undoes `predictor` over one chunk of `cols x rows` pixels at `spp` samples each, in place.
///
/// `samples` is the chunk's unpacked, interleaved sample buffer; `bits` selects the width the
/// addition wraps at. Rows are independent, so this is equally correct per strip, per tile, or
/// per sub-tile — matching the SDK, which applies it to whichever buffer it just decoded.
pub(crate) fn undo(
    predictor: Predictor,
    samples: &mut [u16],
    cols: usize,
    rows: usize,
    spp: usize,
    bits: u16,
) {
    let Some(factor) = x_factor(predictor).filter(|&f| f > 0) else {
        return;
    };
    // The SDK reindexes a group of `x_factor` adjacent pixels as one super-pixel with
    // `spp * x_factor` channels, which makes the back-reference exactly `x_factor` pixels left.
    let channels = spp * factor;
    let group_cols = cols / factor;
    if channels == 0 || group_cols < 2 {
        return;
    }
    let row_step = group_cols * channels;
    for row in 0..rows {
        let base = row * row_step;
        let Some(row_samples) = samples.get_mut(base..base + row_step) else {
            return; // a short buffer is the caller's error to report, not ours to index past
        };
        for i in channels..row_step {
            let previous = row_samples[i - channels];
            row_samples[i] = wrapping_add(row_samples[i], previous, bits);
        }
    }
}

/// Adds at the sample's container width: 8-bit data wraps modulo 256, everything else modulo
/// 2^16 (`DecodeDelta8` vs `DecodeDelta16`).
fn wrapping_add(a: u16, b: u16, bits: u16) -> u16 {
    if bits <= 8 {
        u16::from((a as u8).wrapping_add(b as u8))
    } else {
        a.wrapping_add(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base case: one row, one plane, differences against the pixel immediately left.
    #[test]
    fn horizontal_difference_accumulates_along_a_row() {
        let mut samples = [10u16, 1, 2, 3];
        undo(Predictor::HorizontalDifference, &mut samples, 4, 1, 1, 16);
        assert_eq!(samples, [10, 11, 13, 16]);
    }

    /// Rows are independent: the recurrence restarts at each row's first sample rather than
    /// carrying across the row boundary.
    #[test]
    fn rows_do_not_carry_into_each_other() {
        let mut samples = [1u16, 1, 100, 1];
        undo(Predictor::HorizontalDifference, &mut samples, 2, 2, 1, 16);
        assert_eq!(samples, [1, 2, 100, 101], "row 1 must not see row 0's tail");
    }

    /// Interleaved planes each carry their own recurrence, `spp` samples apart.
    #[test]
    fn planes_are_differenced_independently() {
        // Two pixels, three samples each: [r0,g0,b0, dr,dg,db].
        let mut samples = [10u16, 20, 30, 1, 2, 3];
        undo(Predictor::HorizontalDifference, &mut samples, 2, 1, 3, 16);
        assert_eq!(samples, [10, 20, 30, 11, 22, 33]);
    }

    /// X2 differences against the pixel *two* to the left (DNG 1.7.1 p. 22), which for one plane
    /// means two independent interleaved recurrences.
    #[test]
    fn x2_differences_against_two_pixels_left() {
        let mut samples = [5u16, 7, 1, 2, 3, 4];
        undo(Predictor::HorizontalDifferenceX2, &mut samples, 6, 1, 1, 16);
        // even indices: 5, 5+1=6, 6+3=9 | odd indices: 7, 7+2=9, 9+4=13
        assert_eq!(samples, [5, 7, 6, 9, 9, 13]);
    }

    /// X4 differences against the pixel four to the left.
    #[test]
    fn x4_differences_against_four_pixels_left() {
        let mut samples = [1u16, 2, 3, 4, 10, 20, 30, 40];
        undo(Predictor::HorizontalDifferenceX4, &mut samples, 8, 1, 1, 16);
        assert_eq!(samples, [1, 2, 3, 4, 11, 22, 33, 44]);
    }

    /// 8-bit data wraps modulo 256, as `DecodeDelta8` does — not modulo 65536.
    #[test]
    fn eight_bit_addition_wraps_at_the_byte() {
        let mut samples = [200u16, 100];
        undo(Predictor::HorizontalDifference, &mut samples, 2, 1, 1, 8);
        assert_eq!(samples, [200, 44], "300 mod 256");
        let mut wide = [200u16, 100];
        undo(Predictor::HorizontalDifference, &mut wide, 2, 1, 1, 16);
        assert_eq!(wide, [200, 300], "16-bit data must not wrap at the byte");
    }

    /// A null predictor leaves the buffer untouched.
    #[test]
    fn null_predictor_is_a_no_op() {
        let mut samples = [1u16, 2, 3];
        undo(Predictor::None, &mut samples, 3, 1, 1, 16);
        assert_eq!(samples, [1, 2, 3]);
    }

    /// A single-column chunk has nothing to difference against.
    #[test]
    fn single_column_is_a_no_op() {
        let mut samples = [7u16, 9];
        undo(Predictor::HorizontalDifference, &mut samples, 1, 2, 1, 16);
        assert_eq!(samples, [7, 9]);
    }

    #[test]
    fn absent_tag_means_no_predictor() {
        assert_eq!(
            validate(None, Compression::Deflate, 16).expect("absent"),
            Predictor::None
        );
    }

    /// The integer differencers are accepted for the schemes that do not predict internally.
    #[test]
    fn integer_predictors_are_accepted_for_deflate_and_uncompressed() {
        for compression in [Compression::Deflate, Compression::Uncompressed] {
            for (code, want) in [
                (2u32, Predictor::HorizontalDifference),
                (34892, Predictor::HorizontalDifferenceX2),
                (34893, Predictor::HorizontalDifferenceX4),
            ] {
                assert_eq!(
                    validate(Some(code), compression, 16).expect("accepted"),
                    want,
                    "{compression:?} {code}"
                );
            }
        }
    }

    /// Lossless JPEG and JPEG XL predict internally, so tag 317 is ignored for them rather than
    /// applied or rejected — matching the SDK reader, which never inspects it there.
    #[test]
    fn self_predicting_schemes_ignore_the_tag() {
        for compression in [Compression::LosslessJpeg, Compression::JpegXl] {
            assert_eq!(
                validate(Some(2), compression, 16).expect("ignored"),
                Predictor::None,
                "{compression:?}"
            );
        }
    }

    /// Floating-point predictors belong to float samples, which this crate rejects outright.
    #[test]
    fn floating_point_predictors_are_rejected() {
        for code in [3u32, 34894, 34895] {
            let error = validate(Some(code), Compression::Deflate, 16).expect_err("rejected");
            assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported, "{code}");
        }
    }

    /// Sub-byte depths have no container width to wrap at, so a predictor over them is refused
    /// rather than silently applied at the wrong modulus.
    #[test]
    fn predictors_need_a_whole_container_width() {
        for bits in [10u16, 12, 14] {
            let error = validate(Some(2), Compression::Deflate, bits).expect_err("rejected");
            assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported, "{bits}");
        }
        assert!(validate(Some(2), Compression::Deflate, 8).is_ok());
    }

    #[test]
    fn unknown_predictor_codes_are_invalid_input() {
        let error = validate(Some(9999), Compression::Deflate, 16).expect_err("rejected");
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
    }
}
