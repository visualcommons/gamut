//! Colour-managed decoding to sRGB (the `cmm` feature): [`JpegDecoder::decode_srgb`].
//!
//! A JPEG's pixels mean what its embedded ICC profile says they mean. The plain decode presents
//! the stored samples and leaves the profile to the caller ([`crate::metadata`]); this module
//! closes the loop for the common destination — sRGB `Rgb8` — by handing the profile and
//! the decoded samples to the workspace's transform engine, `gamut-cmm`, against
//! `gamut-icc`'s built-in sRGB profile. Nothing colour-specific is computed here, so an
//! [`XYB`](crate::JpegColorMode::Xyb) stream (gamut's or jpegli's) is not a special case: its
//! profile's `A2B0` pipeline *is* the XYB inverse, and the CMM evaluates it like any other.
//!
//! The transform runs at the **media-relative colorimetric** intent with black-point compensation
//! off, otherwise at `gamut-cmm`'s defaults (`f64` stage-by-stage evaluation, no pipeline
//! optimization). Relative colorimetric is what makes the result the profile's own colorimetry: an
//! input profile carrying only `A2B0` (the XYB profile, and most camera/scanner profiles) is read
//! through that table under every intent (lcms2's perceptual fallback, which `gamut-cmm` follows),
//! but the perceptual intent additionally **forces** black-point compensation whenever the
//! destination is a v4 profile (lcms2's `_cmsLinkProfiles` rule, mirrored by
//! `IccTransform::between`) — and `gamut-icc`'s built-in sRGB is v4. Measured on the XYB profile
//! over a 18³ sRGB grid of exact (unquantized) scaled-XYB inputs: relative colorimetric reproduces
//! the source within 0.44 of an 8-bit code (mean 0.03), perceptual misses by up to 11.2 codes (mean
//! 1.62) as the compensation pulls the dark end down (sRGB grey 20 comes out near 12). libjxl's own tools evaluate the table directly
//! (skcms has no compensation), so relative colorimetric is also what a jpegli pipeline shows.
//!
//! Neither the profile → sRGB choice nor the row-at-a-time in-place loop below is JPEG-specific;
//! moving both into `gamut-cmm`, where the other format crates can share them, is issue #1042.

use gamut_cmm::{IccTransform, TransformOptions, transform_interleaved_u8};
use gamut_core::{DecodeImage, Error, ImageBuf, PixelFormat, Result, Rgb8};
use gamut_icc::{BuiltinProfile, ColorSpace, IccProfile, RenderingIntent};

use crate::JpegDecoder;

impl JpegDecoder {
    /// Decodes `data` to **sRGB** [`Rgb8`], applying the stream's embedded ICC profile through
    /// `gamut-cmm`.
    ///
    /// - **No embedded profile:** the stream is taken to be sRGB already (the JFIF/web
    ///   convention) and the plain [`Rgb8`] decode is returned unchanged.
    /// - **An RGB-data profile:** the decoded samples are transformed from that profile to
    ///   `gamut-icc`'s built-in sRGB profile at the media-relative colorimetric intent without
    ///   black-point compensation (module docs explain why not perceptual). This
    ///   covers the [`XYB`](crate::JpegColorMode::Xyb) colour mode, whose profile carries the XYB
    ///   inverse — gamut's [`XYB_ICC_PROFILE`](crate::XYB_ICC_PROFILE) and libjxl's alike.
    ///
    /// The decode itself is this decoder's [`DecodeImage<Rgb8>`] — its limits, backends and
    /// [`convert_policy`](Self::convert_policy) all apply — and a one-component stream carrying
    /// an RGB profile is transformed after grayscale replication.
    ///
    /// # Errors
    ///
    /// Whatever the [`Rgb8`] decode or [`crate::metadata`] returns; [`Error::InvalidInput`] when
    /// the embedded profile does not parse or `gamut-cmm` cannot build a transform from it; and
    /// [`Error::Unsupported`] for a profile whose data colour space is not RGB (a GRAY profile on
    /// a grayscale stream, a CMYK one on a four-component stream) — those destinations are not
    /// wired here yet.
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    /// use gamut_jpeg::{JpegColorMode, JpegDecoder, JpegEncoder};
    ///
    /// let rgb = vec![200u8; 16 * 16 * 3];
    /// let image = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16)?)?;
    /// let jpeg = JpegEncoder::new()
    ///     .with_color_mode(JpegColorMode::Xyb)
    ///     .encode_to_vec(image)?;
    /// let srgb = JpegDecoder::new().decode_srgb(&jpeg)?;
    /// assert!(srgb.as_samples().iter().all(|&s| s.abs_diff(200) <= 3));
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    pub fn decode_srgb(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        // The profile is read and its transform built before any pixel is decoded, so a profile
        // this path cannot apply costs a header walk, not a decode.
        let transform = match crate::metadata(data)?.icc {
            Some(icc) => Some(srgb_transform(&icc)?),
            None => None,
        };
        let mut image: ImageBuf<Rgb8> = self.decode_image(data)?;
        let Some(transform) = transform else {
            return Ok(image);
        };
        // In place, one row at a time through a row-sized scratch copy: the CMM takes distinct
        // source and destination buffers, and a whole second raster would double the peak memory
        // the decoder's limits bound.
        let stride = image.width() as usize * 3;
        let mut scratch = vec![0u8; stride];
        for row in image.as_mut_samples().chunks_exact_mut(stride) {
            scratch.copy_from_slice(row);
            transform_interleaved_u8(
                &transform,
                PixelFormat::Rgb8,
                &scratch,
                PixelFormat::Rgb8,
                row,
            )
            .map_err(|e| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: the colour transform failed on the decoded samples",
                )
                .with_detail(e.to_string())
            })?;
        }
        Ok(image)
    }
}

/// The transform from the embedded profile `icc` to built-in sRGB.
fn srgb_transform(icc: &[u8]) -> Result<IccTransform> {
    let source = IccProfile::parse(icc).map_err(|e| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: the embedded ICC profile does not parse",
        )
        .with_detail(e.to_string())
    })?;
    if source.header.data_color_space != ColorSpace::Rgb {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: sRGB decoding applies only RGB-data ICC profiles",
        ));
    }
    let srgb = IccProfile::builtin(BuiltinProfile::Srgb).ok_or_else(|| {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: gamut-icc has no built-in sRGB profile",
        )
    })?;
    // Black-point compensation stays at its default, off; only the perceptual intent's v4 rule
    // would force it on (module docs).
    let options = TransformOptions {
        intent: RenderingIntent::MediaRelativeColorimetric,
        ..TransformOptions::default()
    };
    IccTransform::between(&source, &srgb, options).map_err(|e| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: no colour transform can be built from the embedded ICC profile",
        )
        .with_detail(e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef};
    use gamut_icc::{DeviceClass, IccWriter, ProfileHeader};

    use super::*;
    use crate::JpegEncoder;

    /// An RGB gradient (JPEG-friendly content).
    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).flat_map(move |x| [(x * 16) as u8, (y * 16) as u8, 128]))
            .collect()
    }

    fn rgb_jpeg(icc: Option<&[u8]>) -> Vec<u8> {
        let src = gradient(16, 16);
        let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(16, 16).unwrap()).unwrap();
        let mut encoder = JpegEncoder::new().with_quality(90);
        if let Some(icc) = icc {
            encoder = encoder.with_icc_profile(icc);
        }
        encoder.encode_to_vec(img).unwrap()
    }

    #[test]
    fn a_stream_without_a_profile_is_returned_as_decoded() {
        let jpeg = rgb_jpeg(None);
        let plain: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(JpegDecoder::new().decode_srgb(&jpeg).unwrap(), plain);
    }

    #[test]
    fn an_xyb_stream_decodes_to_its_dark_source_colour() {
        // Pins that the profile is applied (the plain decode returns scaled-XYB bytes, nowhere
        // near the source) and that it is applied colorimetrically: the perceptual intent's forced
        // black-point compensation crushes this dark grey from 20 to 11 (module docs).
        let src = [20u8; 16 * 16 * 3];
        let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(16, 16).unwrap()).unwrap();
        let jpeg = JpegEncoder::new()
            .with_color_mode(crate::JpegColorMode::Xyb)
            .with_quality(95)
            .encode_to_vec(img)
            .unwrap();
        let srgb = JpegDecoder::new().decode_srgb(&jpeg).unwrap();
        assert_eq!(srgb.dimensions(), img.dimensions());
        let worst = srgb
            .as_samples()
            .iter()
            .map(|s| s.abs_diff(20))
            .max()
            .unwrap();
        // Measured: 1 code here, 9 under the perceptual intent. Asserted with margin.
        assert!(worst <= 3, "dark grey moved by {worst} codes");
    }

    #[test]
    fn a_non_rgb_profile_is_unsupported() {
        let gray = IccWriter::new()
            .write(&IccProfile::gray_with_gamma(2.2).unwrap())
            .unwrap();
        let px = vec![100u8; 64];
        let img = ImageRef::<Gray8>::new(&px, Dimensions::new(8, 8).unwrap()).unwrap();
        let jpeg = JpegEncoder::new()
            .with_icc_profile(&gray)
            .encode_to_vec(img)
            .unwrap();
        let err = JpegDecoder::new().decode_srgb(&jpeg).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{err:?}");
    }

    #[test]
    fn an_unparsable_profile_is_invalid_input() {
        let err = JpegDecoder::new()
            .decode_srgb(&rgb_jpeg(Some(&[0u8; 132])))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
    }

    #[test]
    fn a_profile_with_no_transform_is_invalid_input() {
        // A well-formed RGB display profile with no tags at all: it parses, but there is no
        // device→PCS pipeline for the CMM to build.
        let empty = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: Vec::new(),
        };
        let bytes = IccWriter::new().write(&empty).unwrap();
        let err = JpegDecoder::new()
            .decode_srgb(&rgb_jpeg(Some(&bytes)))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
    }
}
