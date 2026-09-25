//! The one AVIF fixture every direction is measured on, plus the two gamut encodes that produce
//! it. Shared so a difference between two test files can only come from what they assert, never
//! from a different source image.
#![allow(dead_code)] // each integration-test binary uses a different subset

use c2pa_oracle::{OracleError, Result};
use gamut_avif::AvifEncoder;
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};

/// Fixture width. Deliberately not a multiple of 16, so the AV1 tile padding is exercised and a
/// container that mis-sizes a box cannot pass by luck.
pub const W: u32 = 34;
/// Fixture height, chosen with [`W`] for the same reason.
pub const H: u32 = 18;

/// The fixture's dimensions.
pub fn dims() -> Dimensions {
    Dimensions {
        width: W,
        height: H,
    }
}

/// A structured source: every channel varies along both axes, so a byte moved by a container bug
/// changes the decoded image rather than landing in a uniform run.
pub fn source_rgb() -> Vec<u8> {
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            rgb[i] = ((x * 7 + y * 3) & 0xff) as u8;
            rgb[i + 1] = ((x * x + y) & 0xff) as u8;
            rgb[i + 2] = ((x ^ (y * 5)) & 0xff) as u8;
        }
    }
    rgb
}

/// The fixture encoded with no C2PA box at all: the asset a claim generator is handed.
///
/// # Panics
///
/// If the fixture does not encode, which would be a `gamut-avif` defect unrelated to C2PA and is
/// not what any test here is measuring.
pub fn plain_avif() -> Vec<u8> {
    let rgb = source_rgb();
    AvifEncoder::new()
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims()).expect("buffer matches dimensions"))
        .expect("the fixture encodes")
}

/// The fixture encoded with a `len`-byte **reserved** C2PA slot, with the byte range
/// `AvifEncoder::encode_with_report` says the slot occupies.
///
/// Shaped to be handed straight to `c2pa_oracle::reserve_then_fill`.
///
/// # Errors
///
/// [`OracleError::Asset`] if the encode fails or reports no range for a slot it was asked to
/// reserve.
pub fn reserve_avif(len: usize) -> Result<(Vec<u8>, std::ops::Range<usize>)> {
    let rgb = source_rgb();
    let (bytes, report) = AvifEncoder::new()
        .with_c2pa_reserved(len)
        .encode_with_report(ImageRef::<Rgb8>::new(&rgb, dims()).expect("buffer matches dimensions"))
        .map_err(|error| OracleError::Asset(error.to_string()))?;
    let range = report.c2pa.ok_or_else(|| {
        OracleError::Asset("encode_with_report reported no range for the reserved slot".into())
    })?;
    Ok((bytes, range))
}
