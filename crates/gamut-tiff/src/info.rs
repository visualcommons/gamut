//! What a TIFF page declares about its stored pixels, read from tags alone.
//!
//! This module *describes*; it does not judge. Which depths, sample formats and photometric
//! interpretations this crate can actually decode is policy, and policy lives in the decoder — so a
//! page `gamut-tiff` cannot decode can still be inspected here. Keeping the two apart is what lets
//! a caller dispatch on a page's declared layout before committing to a decode, and it keeps the
//! defaults for absent tags in one place instead of drifting between the probe and the decoder.

use gamut_core::{Error, Result};
use gamut_ifd::{ByteOrder, Ifd};

use crate::compression::{Compression, tiff_compression};
use crate::ifd::{
    PhotometricInterpretation, Predictor, SampleFormat, tiff_photometric, tiff_predictor,
    tiff_sample_format,
};
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
    /// The first value of `ExtraSamples` (338): what the first component past the photometric
    /// interpretation's own carries, or `None` when the tag is absent (or empty).
    ///
    /// The first is the one component this crate presents as alpha; a page with more extra
    /// samples than one is described here but refused by the decoder. Use
    /// [`deconstruct`](crate::deconstruct) to read the whole list.
    pub extra_sample: Option<ExtraSample>,
}

/// What an extra component carries, stored in the `ExtraSamples` tag (338, TIFF 6.0 §18).
///
/// The discriminant is the on-disk code; it converts back with `u16::from` and parses with
/// `TryFrom<u16>`, which refuses a code §18 does not assign.
///
/// How [`TiffDecoder`](crate::TiffDecoder) presents each code follows libtiff's RGBA reader
/// (`tif_getimage.c`), the crate's oracle: associated alpha is un-premultiplied into the
/// unassociated `Rgba*`/`GrayAlpha*` layouts and unassociated alpha passes through. On a
/// four-sample RGB page, [`Unspecified`](Self::Unspecified) — like an absent `ExtraSamples` tag —
/// is read as associated alpha, the reading libtiff gives such files; on a two-sample grey page,
/// where libtiff gives it no alpha reading, it is refused.
///
/// `#[non_exhaustive]`: later specifications may assign further codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
#[non_exhaustive]
pub enum ExtraSample {
    /// `0` — unspecified data.
    Unspecified = 0,
    /// `1` — associated alpha: every colour sample is already multiplied by it (premultiplied).
    AssociatedAlpha = 1,
    /// `2` — unassociated alpha: the colour samples are independent of it (straight).
    UnassociatedAlpha = 2,
}

impl TryFrom<u16> for ExtraSample {
    type Error = Error;

    /// Maps an on-disk `ExtraSamples` code to its meaning.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code past
    /// §18's three.
    fn try_from(code: u16) -> Result<Self> {
        Ok(match code {
            0 => ExtraSample::Unspecified,
            1 => ExtraSample::AssociatedAlpha,
            2 => ExtraSample::UnassociatedAlpha,
            _ => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unrecognised ExtraSamples tag value",
                ));
            }
        })
    }
}

impl From<ExtraSample> for u16 {
    /// The on-disk code, the `SHORT` written to tag 338.
    fn from(sample: ExtraSample) -> Self {
        sample as u16
    }
}

/// Reads one page's pixel-layout tags, applying TIFF's defaults for the absent ones.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if a
/// required tag is missing, the page is zero-sized, or `SamplesPerPixel` is 0, or an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for an on-disk code this crate does not recognise or a layout a single
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

    let compression = tiff_compression(ifd.get_u32(tags::COMPRESSION).unwrap_or(1))?;
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
    //
    // The absent tag's default (one bit for every sample) is applied as the single depth it is,
    // never materialised as a per-sample list: `SamplesPerPixel` is an untrusted count up to
    // `u32::MAX`, and a list sized by it is an allocation no byte of the file backs (#949: a
    // 173-byte file declaring 4 294 967 209 samples asked for 16 GiB). A list that *is* present
    // was read from the file, so its length is already bounded by the input.
    let bits_per_sample = match ifd.get_u32_vec(tags::BITS_PER_SAMPLE) {
        None => 1,
        Some(bits) => match bits.first() {
            Some(&first)
                if bits.len() == samples_per_pixel as usize && bits.iter().all(|&b| b == first) =>
            {
                first
            }
            _ => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: mixed bit depths not supported",
                ));
            }
        },
    };

    // SampleFormat (339) is likewise one value per sample. Writers commonly store a single value
    // for all samples, so the count is deliberately not checked against SamplesPerPixel — only
    // samples that disagree with each other are refused, since one value cannot describe them.
    let formats = ifd.get_u32_vec(tags::SAMPLE_FORMAT).unwrap_or_default();
    let sample_format = match formats.first() {
        None => SampleFormat::UnsignedInteger,
        Some(&first) if formats.iter().all(|&f| f == first) => tiff_sample_format(first)?,
        Some(_) => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: mixed sample formats not supported",
            ));
        }
    };

    let photometric = tiff_photometric(require_u32(
        ifd,
        tags::PHOTOMETRIC_INTERPRETATION,
        "TIFF: missing PhotometricInterpretation",
    )?)?;
    let predictor = tiff_predictor(ifd.get_u32(tags::PREDICTOR).unwrap_or(1))?;
    // ExtraSamples (338) is one value per extra component; only the first is ever presented, so
    // only the first is read. A code past §18's three is refused like every other unrecognised
    // code here — no conformant file carries one, and guessing would choose a colour reading.
    let extra_sample = match ifd.get_u32_vec(tags::EXTRA_SAMPLES).as_deref() {
        Some([first, ..]) => Some(crate::ifd::read(
            *first,
            |_: &ExtraSample| true,
            "TIFF: unrecognised ExtraSamples tag value",
        )?),
        _ => None,
    };

    Ok(TiffInfo {
        width,
        height,
        bits_per_sample,
        sample_format,
        photometric,
        samples_per_pixel,
        compression,
        predictor,
        tiled: ifd.get(tags::TILE_WIDTH).is_some(),
        byte_order,
        extra_sample,
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

    /// A page with either side zero is refused as zero-sized, each side on its own: a zero
    /// width with a non-zero height (and the reverse) must not slip past the check.
    #[test]
    fn page_info_rejects_a_zero_width_or_a_zero_height() {
        for (width, height) in [(0, 1), (1, 0)] {
            let mut ifd = Ifd::new();
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![width]));
            ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![height]));
            ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![1]));
            let err = page_info(&ifd, ByteOrder::LittleEndian).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{width}x{height}");
            assert_eq!(
                err.static_message(),
                Some("TIFF: zero-sized image"),
                "{width}x{height}"
            );
        }
    }

    /// Every code §18 assigns writes and reads back as itself; the next one is refused.
    #[test]
    fn extra_sample_codes_are_section_18s_three() {
        for (sample, code) in [
            (ExtraSample::Unspecified, 0),
            (ExtraSample::AssociatedAlpha, 1),
            (ExtraSample::UnassociatedAlpha, 2),
        ] {
            assert_eq!(u16::from(sample), code);
            assert_eq!(ExtraSample::try_from(code).unwrap(), sample);
        }
        let err = ExtraSample::try_from(3).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }

    /// A 1x1 RGB page carrying `extra` as its `ExtraSamples` field.
    fn extra_samples_ifd(extra: Vec<u16>) -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
        ifd.set(tags::EXTRA_SAMPLES, Value::Short(extra));
        ifd
    }

    /// Only the first `ExtraSamples` value is reported; an empty list reads as no tag, and a code
    /// past §18's three is refused, even in second place it would never be read from.
    #[test]
    fn page_info_reports_the_first_extra_sample() {
        let info = |extra| page_info(&extra_samples_ifd(extra), ByteOrder::LittleEndian);
        assert_eq!(
            info(vec![1, 0]).unwrap().extra_sample,
            Some(ExtraSample::AssociatedAlpha)
        );
        assert_eq!(info(vec![]).unwrap().extra_sample, None);
        let err = info(vec![3]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("TIFF: unrecognised ExtraSamples tag value")
        );
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
