//! Presentation: the colour transform the stream declares (see the parent module's
//! [Colour interpretation](super#colour-interpretation) section), the row loop that writes the
//! native layout through each component's upsampling filter, and the [`DecodeImage`] impls that
//! hand it to [`gamut_core::convert`] for any other layout.

use gamut_color::{ChromaUpsampling, ColorRange, bt601_ycbcr_to_rgb};
use gamut_core::convert::{RawImage, convert_from_raw, convert_from_raw_into};
use gamut_core::{
    Cmyk8, DecodeImage, Dimensions, Error, Gray8, ImageBuf, Pixel, PixelFormat, Result, Rgb8,
};

use super::upsample::Filter;
use super::{DecodedImage, JpegDecoder};

/// Converts one row of full-range YCbCr (T.871 §7) to interleaved RGB in `out`, three samples per
/// pixel, through `packed`, a scratch row of at least as many pixels.
///
/// The arithmetic is [`bt601_ycbcr_to_rgb`]'s, sample for sample, so the output is byte-identical to the
/// per-pixel conversion; what changes is the shape of the loops, so that the compiler vectorises
/// them with the target's baseline vector instructions (SSE2 on x86-64, NEON on aarch64) and no
/// `unsafe`:
///
/// 1. The conversion runs over whole rows with nothing but the three source samples and one
///    destination per pixel, packing each result into a `u32` lane (`R`, `G`, `B`, then a zero
///    byte). Converting straight into the interleaved row instead left it scalar: its three byte
///    stores per pixel are a stride-3 pattern the vectoriser does not take.
/// 2. The packed row is written out with one four-byte store per pixel, each overlapping the next
///    pixel's first byte, which that pixel's own store then overwrites. Only the last pixel, whose
///    fourth byte would land past the row, is written as three bytes.
fn ycbcr_to_rgb_row(y: &[u8], cb: &[u8], cr: &[u8], packed: &mut [u32], out: &mut [u8]) {
    for (((p, &y), &cb), &cr) in packed.iter_mut().zip(y).zip(cb).zip(cr) {
        let (r, g, b) = bt601_ycbcr_to_rgb(y, cb, cr, ColorRange::Full);
        *p = u32::from_le_bytes([r, g, b, 0]);
    }
    let n = out.len() / 3;
    for (i, p) in packed.iter().enumerate().take(n.saturating_sub(1)) {
        out[3 * i..3 * i + 4].copy_from_slice(&p.to_le_bytes());
    }
    if let (Some(px), Some(p)) = (
        out.as_chunks_mut::<3>().0.last_mut(),
        packed.get(n.wrapping_sub(1)),
    ) {
        let [r, g, b, _] = p.to_le_bytes();
        *px = [r, g, b];
    }
}

/// The colour transform selected for presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transform {
    /// One luminance channel.
    Gray,
    /// Three channels stored directly as R, G, B.
    Rgb,
    /// Three channels YCbCr → RGB (T.871 §7).
    YCbCr,
    /// Four channels stored directly as C, M, Y, K.
    Cmyk,
    /// Four channels YCCK: inverse YCbCr on the first three, `K` passed through.
    Ycck,
}

/// The colour-interpretation decision tree (see the module docs).
fn decide_transform(img: &DecodedImage) -> Result<Transform> {
    match img.comps.len() {
        1 => Ok(Transform::Gray),
        3 => Ok(match img.color.adobe_transform {
            Some(0) => Transform::Rgb,
            Some(_) => Transform::YCbCr, // 1 = YCbCr; any other Adobe value defaults to YCbCr
            None => {
                if img.color.jfif {
                    Transform::YCbCr
                } else if img.ids.as_slice() == b"RGB" {
                    Transform::Rgb
                } else {
                    Transform::YCbCr
                }
            }
        }),
        4 => Ok(match img.color.adobe_transform {
            Some(2) => Transform::Ycck,
            _ => Transform::Cmyk,
        }),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: only 1, 3, or 4 component streams are supported",
        )),
    }
}

/// Presents a decoded image in the layout its components natively describe, applying **only** the
/// colour transform the stream declares: YCbCr → RGB, and YCCK → CMYK (Adobe TN #5116). Rearranging
/// that into the caller's pixel layout is [`gamut_core::convert`]'s job, so this function never
/// replicates grey across channels or refuses a target.
///
/// Works a row at a time: each component's row is upsampled through its [`Filter`] (in place at
/// full resolution, into a scratch row otherwise), and the transform then runs over the zipped
/// rows, writing straight into `out` -- `width * height` pixels of `format` as
/// [`native_layout`] returned it alongside `transform`. The caller sizes `out`, so the presented
/// image is written once, into the buffer that is returned. YCbCr rows, the common case, convert
/// through [`ycbcr_to_rgb_row`].
fn present_native(
    img: &DecodedImage,
    upsampling: ChromaUpsampling,
    transform: Transform,
    format: PixelFormat,
    out: &mut [u8],
) {
    let w = img.width as usize;
    let filters: Vec<Filter> = img
        .comps
        .iter()
        .map(|c| {
            Filter::new(
                (usize::from(c.h), img.hmax),
                (usize::from(c.v), img.vmax),
                w,
                upsampling,
            )
        })
        .collect();
    let mut scratch = vec![vec![0u8; w]; img.comps.len()];
    let mut packed = vec![0u32; w];
    for (py, line) in out.chunks_exact_mut(w * format.channels()).enumerate() {
        let rows: Vec<&[u8]> = filters
            .iter()
            .zip(&mut scratch)
            .enumerate()
            .map(|(ci, (filter, scratch))| filter.row(img, ci, py, scratch))
            .collect();
        match (transform, rows.as_slice()) {
            (Transform::Gray, &[y]) => line.copy_from_slice(y),
            (Transform::Rgb, &[r, g, b]) => {
                for (((px, &r), &g), &b) in
                    line.as_chunks_mut::<3>().0.iter_mut().zip(r).zip(g).zip(b)
                {
                    px.copy_from_slice(&[r, g, b]);
                }
            }
            (Transform::YCbCr, &[y, cb, cr]) => ycbcr_to_rgb_row(y, cb, cr, &mut packed, line),
            (Transform::Cmyk, &[c, m, y, k]) => {
                for ((((px, &c), &m), &y), &k) in line
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(c)
                    .zip(m)
                    .zip(y)
                    .zip(k)
                {
                    px.copy_from_slice(&[c, m, y, k]);
                }
            }
            // YCCK → CMYK: invert the YCbCr transform on the first three channels, K passes
            // through (Adobe TN #5116).
            (Transform::Ycck, &[y, cb, cr, k]) => {
                for ((((px, &y), &cb), &cr), &k) in line
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(y)
                    .zip(cb)
                    .zip(cr)
                    .zip(k)
                {
                    let (r, g, b) = bt601_ycbcr_to_rgb(y, cb, cr, ColorRange::Full);
                    px.copy_from_slice(&[255 - r, 255 - g, 255 - b, k]);
                }
            }
            // `decide_transform` picks each transform from the component count, so the row count
            // always matches the arm; this one is unreachable and stays only to keep the match
            // total without an `unwrap`.
            _ => {}
        }
    }
}

/// The colour transform `img` needs and the pixel layout it presents natively, decided before any
/// sample is written: Gray8 for one component, Rgb8 for three, Cmyk8 for four.
fn native_layout(img: &DecodedImage) -> Result<(Transform, PixelFormat)> {
    let transform = decide_transform(img)?;
    let format = match transform {
        Transform::Gray => PixelFormat::Gray8,
        Transform::Rgb | Transform::YCbCr => PixelFormat::Rgb8,
        Transform::Cmyk | Transform::Ycck => PixelFormat::Cmyk8,
    };
    Ok((transform, format))
}

impl JpegDecoder {
    /// Decodes `data`, through a backend when one accepts it, and presents it as layout `P`.
    fn present<P: Pixel<Sample = u8>>(&self, data: &[u8]) -> Result<ImageBuf<P>> {
        if let Some(img) = self.decode_via_backend(data)? {
            let dims = Dimensions::new(img.width(), img.height())?;
            let raw = RawImage::new(img.samples(), img.format(), dims)?;
            return convert_from_raw(raw, self.policy);
        }
        let img = self.decode_internal(data)?;
        let dims = Dimensions::new(img.width, img.height)?;
        let (transform, format) = native_layout(&img)?;
        self.present_decoded(&img, dims, transform, format)
    }

    /// Presents the built-in decoder's `img` as layout `P` in a new buffer. When `P` is the native
    /// layout the samples are written straight into the returned image; otherwise they are written
    /// once natively and converted by [`gamut_core::convert`] under the decoder's policy (the
    /// identity a matching layout would take there needs no permission, so writing it directly
    /// changes no outcome).
    fn present_decoded<P: Pixel<Sample = u8>>(
        &self,
        img: &DecodedImage,
        dims: Dimensions,
        transform: Transform,
        format: PixelFormat,
    ) -> Result<ImageBuf<P>> {
        if format == P::FORMAT {
            let mut out = ImageBuf::<P>::zeroed(dims)?;
            present_native(
                img,
                self.upsampling,
                transform,
                format,
                out.as_mut_samples(),
            );
            Ok(out)
        } else {
            let samples = native_samples(img, self.upsampling, transform, format);
            convert_from_raw(RawImage::new(&samples, format, dims)?, self.policy)
        }
    }

    /// As [`JpegDecoder::present`], reusing `dst`'s sample storage when the decoded dimensions
    /// match its own. On error `dst` is left unchanged.
    fn present_into<P: Pixel<Sample = u8>>(
        &self,
        data: &[u8],
        dst: &mut ImageBuf<P>,
    ) -> Result<()> {
        if let Some(img) = self.decode_via_backend(data)? {
            let dims = Dimensions::new(img.width(), img.height())?;
            return self.convert_into(RawImage::new(img.samples(), img.format(), dims)?, dst);
        }
        let img = self.decode_internal(data)?;
        let dims = Dimensions::new(img.width, img.height)?;
        let (transform, format) = native_layout(&img)?;
        if format != P::FORMAT {
            let samples = native_samples(&img, self.upsampling, transform, format);
            return self.convert_into(RawImage::new(&samples, format, dims)?, dst);
        }
        // The native layout is the requested one: write it straight into `dst` when the
        // dimensions let it hold the image. Nothing past `native_layout` can fail, so `dst` is
        // only touched once the presentation is certain to complete.
        if dst.dimensions() == dims {
            present_native(
                &img,
                self.upsampling,
                transform,
                format,
                dst.as_mut_samples(),
            );
        } else {
            *dst = self.present_decoded(&img, dims, transform, format)?;
        }
        Ok(())
    }

    /// Converts `raw` into `dst` under the decoder's policy, reusing `dst`'s storage when the
    /// dimensions match and replacing the buffer otherwise. On error `dst` is left unchanged.
    fn convert_into<P: Pixel<Sample = u8>>(
        &self,
        raw: RawImage<'_, u8>,
        dst: &mut ImageBuf<P>,
    ) -> Result<()> {
        if dst.dimensions() == raw.dimensions() {
            convert_from_raw_into::<_, P>(raw, self.policy, dst.as_mut_samples())
        } else {
            *dst = convert_from_raw(raw, self.policy)?;
            Ok(())
        }
    }
}

/// [`present_native`] into a new buffer of exactly the native layout's size, for a caller that
/// converts it on into another layout.
fn native_samples(
    img: &DecodedImage,
    upsampling: ChromaUpsampling,
    transform: Transform,
    format: PixelFormat,
) -> Vec<u8> {
    let mut out = vec![0u8; img.width as usize * img.height as usize * format.channels()];
    present_native(img, upsampling, transform, format, &mut out);
    out
}

impl DecodeImage<Rgb8> for JpegDecoder {
    /// Grayscale is replicated across channels; three-component YCbCr/RGB is presented as RGB;
    /// four-component (CMYK/YCCK) returns an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error (decode as [`Cmyk8`]).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Rgb8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

impl DecodeImage<Gray8> for JpegDecoder {
    /// A single-component stream passes through unchanged. A colour stream needs a
    /// [`LumaPolicy`](gamut_core::convert::LumaPolicy) to be reduced to luma.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Gray8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

impl DecodeImage<Cmyk8> for JpegDecoder {
    /// Errors unless the stream is four-component; CMYK passes through, YCCK is inverted to CMYK.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Cmyk8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Cmyk8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ycbcr_row_matches_the_per_pixel_conversion() {
        // Short rows (where the overlapping stores and the last pixel's three-byte store meet) and
        // rows past a vector's width, with the three planes varied independently so a lane, plane
        // or byte-order mix-up moves a sample, and a scratch row holding junk so every pixel's
        // packed value must be rewritten before it is stored.
        for len in [0usize, 1, 2, 3, 4, 5, 15, 16, 17, 33] {
            let y: Vec<u8> = (0..len).map(|i| (i * 97 + 3) as u8).collect();
            let cb: Vec<u8> = (0..len).map(|i| (i * 53 + 200) as u8).collect();
            let cr: Vec<u8> = (0..len).map(|i| (i * 29 + 90) as u8).collect();
            let want: Vec<u8> = (0..len)
                .flat_map(|i| {
                    let (r, g, b) = bt601_ycbcr_to_rgb(y[i], cb[i], cr[i], ColorRange::Full);
                    [r, g, b]
                })
                .collect();
            let mut packed = vec![u32::MAX; len];
            let mut out = vec![0u8; 3 * len];
            ycbcr_to_rgb_row(&y, &cb, &cr, &mut packed, &mut out);
            assert_eq!(out, want, "len {len}");
        }
    }
}
