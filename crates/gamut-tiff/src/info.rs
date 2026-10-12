//! What a TIFF page declares about its stored pixels, read from tags alone.
//!
//! This module *describes*; it does not judge. Which depths, sample formats and photometric
//! interpretations this crate can actually decode is policy, and policy lives in the decoder — so a
//! page `gamut-tiff` cannot decode can still be inspected here. Keeping the two apart is what lets
//! a caller dispatch on a page's declared layout before committing to a decode, and it keeps the
//! defaults for absent tags in one place instead of drifting between the probe and the decoder.

use gamut_core::{Error, Result};
use gamut_ifd::{ByteOrder, Ifd};

use crate::compression::Compression;
use crate::ifd::{PhotometricInterpretation, Predictor, SampleFormat};
use crate::tags;

/// Reads a required unsigned-integer tag.
fn require_u32(ifd: &Ifd, tag: u16, what: &'static str) -> Result<u32> {
    ifd.get_u32(tag)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), what))
}

/// What a TIFF page declares about its stored pixels — enough to choose a pixel type, or to decline
/// the file, without paying for a decode.
///
/// Every field is reported **as declared**, with the TIFF 6.0 defaults applied for absent tags
/// (`SamplesPerPixel = 1`, `BitsPerSample = 1`, `Compression = None`, `Predictor = 1`,
/// `SampleFormat = 1`).
///
/// Crucially, a page this crate cannot *decode* can still be described: a 32-bit floating-point
/// page reports its depth and format here and only fails at
/// [`decode_image`](gamut_core::DecodeImage::decode_image). That is the whole point of a probe —
/// support policy belongs to the decoder, so a caller can see what it is dealing with and dispatch
/// (or give a good error of its own) rather than guess from a failure.
///
/// What *is* refused here is a page one value cannot honestly describe — samples that disagree
/// about their depth or format — and an on-disk code this crate does not recognise. Use
/// [`deconstruct`](crate::deconstruct) to inspect unknown codes verbatim.
///
/// Marked `#[non_exhaustive]` so further fields can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TiffInfo {
    /// Image width in pixels (`ImageWidth`, 256).
    pub width: u32,
    /// Image height in pixels (`ImageLength`, 257).
    pub height: u32,
    /// Bits per sample (`BitsPerSample`, 258). Every sample shares one depth.
    pub bits_per_sample: u32,
    /// How the sample bits are interpreted (`SampleFormat`, 339).
    pub sample_format: SampleFormat,
    /// How samples map to colour (`PhotometricInterpretation`, 262).
    pub photometric: PhotometricInterpretation,
    /// Components per pixel (`SamplesPerPixel`, 277).
    pub samples_per_pixel: u32,
    /// The compression scheme (`Compression`, 259).
    pub compression: Compression,
    /// The prediction scheme applied before compression (`Predictor`, 317).
    pub predictor: Predictor,
    /// Whether the page stores tiles (`TileWidth` present) rather than strips.
    pub tiled: bool,
    /// The byte order of the file the page belongs to (`II` or `MM`).
    pub byte_order: ByteOrder,
}

/// Reads one page's pixel-layout tags, applying TIFF's defaults for the absent ones.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if a required tag is missing, the page is zero-sized, or
/// `SamplesPerPixel` is 0, or
/// [`Error::Unsupported`] for an on-disk code this crate does not recognise or a layout a single
/// depth cannot describe.
pub(crate) fn page_info(ifd: &Ifd, byte_order: ByteOrder) -> Result<TiffInfo> {
    let width = require_u32(ifd, tags::IMAGE_WIDTH, "TIFF: missing ImageWidth")?;
    let height = require_u32(ifd, tags::IMAGE_LENGTH, "TIFF: missing ImageLength")?;
    if width == 0 || height == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: zero-sized image",
        ));
    }

    let compression = Compression::try_from(ifd.get_u32(tags::COMPRESSION).unwrap_or(1))?;
    let samples_per_pixel = ifd.get_u32(tags::SAMPLES_PER_PIXEL).unwrap_or(1);
    // TIFF 6.0 defaults SamplesPerPixel (277) to 1 and gives 0 no meaning: a pixel with no
    // components stores nothing. Refused here because every per-sample tag below would otherwise
    // be an empty list, and an empty BitsPerSample passes the agreement check vacuously.
    if samples_per_pixel == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: SamplesPerPixel is 0",
        ));
    }

    // BitsPerSample (258) is one value per sample. This crate's sample model gives every component
    // the same depth, so a page whose samples disagree cannot be described — let alone decoded.
    let bits = ifd
        .get_u32_vec(tags::BITS_PER_SAMPLE)
        .unwrap_or_else(|| vec![1; samples_per_pixel as usize]);
    if bits.len() != samples_per_pixel as usize || bits.iter().any(|&b| b != bits[0]) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: mixed bit depths not supported",
        ));
    }

    // SampleFormat (339) is likewise one value per sample. Writers commonly store a single value
    // for all samples, so the count is deliberately not checked against SamplesPerPixel — only
    // samples that disagree with each other are refused, since one value cannot describe them.
    let formats = ifd.get_u32_vec(tags::SAMPLE_FORMAT).unwrap_or_default();
    let sample_format = match formats.first() {
        None => SampleFormat::UnsignedInteger,
        Some(&first) if formats.iter().all(|&f| f == first) => SampleFormat::try_from(first)?,
        Some(_) => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: mixed sample formats not supported",
            ));
        }
    };

    let photometric = PhotometricInterpretation::try_from(require_u32(
        ifd,
        tags::PHOTOMETRIC_INTERPRETATION,
        "TIFF: missing PhotometricInterpretation",
    )?)?;
    let predictor = Predictor::try_from(ifd.get_u32(tags::PREDICTOR).unwrap_or(1))?;

    Ok(TiffInfo {
        width,
        height,
        bits_per_sample: bits[0],
        sample_format,
        photometric,
        samples_per_pixel,
        compression,
        predictor,
        tiled: ifd.get(tags::TILE_WIDTH).is_some(),
        byte_order,
    })
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;
    use gamut_ifd::Value;

    use super::*;

    /// IFD0 of issue #563's reproducer: a 1x1 BlackIsZero page declaring `SamplesPerPixel = 0`.
    fn zero_samples_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![0]));
        ifd
    }

    #[test]
    fn page_info_rejects_zero_samples_per_pixel_as_invalid_input() {
        let err = page_info(&zero_samples_ifd(), ByteOrder::LittleEndian).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn page_info_rejects_zero_samples_per_pixel_with_empty_bits_per_sample() {
        let mut ifd = zero_samples_ifd();
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![]));
        let err = page_info(&ifd, ByteOrder::LittleEndian).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }
}
