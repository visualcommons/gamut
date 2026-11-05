//! Typed pixel-buffer application: run any [`Transform`] over interleaved or planar
//! `u8`/`u16` image buffers tagged with [`gamut_core::PixelFormat`].
//!
//! Three entry points, each generic over the sample width ([`ImageSample`]: `u8` or `u16`):
//! [`transform_interleaved`] reads a [`RawImage`] (an [`ImageRef`](gamut_core::ImageRef) gives
//! one through `as_raw`) into a caller-owned destination, [`transform_in_place`] rewrites an
//! [`ImageBuf`] without a second raster, and [`transform_planar`] walks one slice per channel.
//! Every format a call names must store samples of the call's width — the same rule
//! [`RawImage::new`] enforces on the source.
//!
//! # Semantics
//!
//! - **Colour channels** go through the transform's integer entry point at the buffer's
//!   width ([`Transform::transform_u8`]/[`Transform::transform_u16`]). For every transform
//!   that does not override it, that means: normalized `v / 255` (8-bit) or `v / 65535`
//!   (16-bit), run through the transform, then re-encoded `round_half_up(v × scale)` clamped
//!   to the sample range — lcms2's `_cmsQuickSaturateWord` rounding (`floor(x + 0.5)`,
//!   saturating), at both widths. A
//!   [`PipelineOptimization::PrecalculateFixed16`](crate::PipelineOptimization::PrecalculateFixed16)
//!   transform interpolates the codes in fixed point instead.
//! - **Alpha channels** ([`ColorModel::Rgba`]/[`ColorModel::GrayAlpha`], alpha stored last)
//!   never enter the transform: when both formats carry alpha it is copied through
//!   untouched; a source-only alpha is dropped; a destination-only alpha is filled with the
//!   full-scale (opaque) value.
//! - The source and destination formats must agree with the transform on **colour** channel
//!   counts and with each other on the **pixel** count — but may differ in colour model
//!   ([`PixelFormat::Rgb8`] → [`PixelFormat::Cmyk8`] is a valid CMM conversion).
//! - [`PixelFormat::Bilevel`] and [`PixelFormat::Indexed8`] are rejected
//!   ([`CmmError::UnsupportedPixelFormat`]): a threshold bit and palette indices are not
//!   continuous colour.
//!
//! # Space/time tradeoff
//!
//! Buffers are processed in fixed chunks of 256 pixels through **one reused pair of
//! colour-only scratch vectors** at the buffer's own sample width (≤ `2 × 256 × 16` samples,
//! 8 or 16 KiB), so peak extra memory is constant in the image size while the transform still
//! sees whole batches rather than single pixels; the provided integer entry points convert
//! through a further 16 KiB of `f64` stack scratch. Planar buffers gather each chunk from the
//! per-channel planes into the same interleaved scratch (and scatter back), trading one extra
//! copy for reuse of the identical conversion core.

use gamut_core::{ColorModel, ImageBuf, Pixel, PixelFormat, RawImage, Sample};

use crate::error::{CmmError, Result};
use crate::transform::Transform;

/// Pixels processed per scratch-buffer refill (the module's space/time tradeoff).
const CHUNK_PIXELS: usize = 256;

/// A sample width the pixel-buffer entry points run at: `u8` or `u16`, each through the
/// [`Transform`] entry point of its own width ([`Transform::transform_u8`] /
/// [`Transform::transform_u16`]).
///
/// Every [`gamut_core::Sample`] is one — the two traits cover the same two types — but the
/// width-specific dispatch is this crate's, so the bound is spelled here. Sealed: it is
/// implemented for `u8` and `u16` and cannot be implemented outside this crate.
pub trait ImageSample: Sample + sealed::Run {}

impl ImageSample for u8 {}
impl ImageSample for u16 {}

mod sealed {
    use crate::error::Result;
    use crate::transform::Transform;

    /// The [`Transform`] entry point at a sample width; the seal on
    /// [`ImageSample`](super::ImageSample).
    pub trait Run: Sized {
        /// Runs gathered colour samples through the transform's entry point at this width.
        fn run(transform: &dyn Transform, src: &[Self], dst: &mut [Self]) -> Result<()>;
    }

    impl Run for u8 {
        fn run(transform: &dyn Transform, src: &[Self], dst: &mut [Self]) -> Result<()> {
            transform.transform_u8(src, dst)
        }
    }

    impl Run for u16 {
        fn run(transform: &dyn Transform, src: &[Self], dst: &mut [Self]) -> Result<()> {
            transform.transform_u16(src, dst)
        }
    }
}

/// A format's shape for the conversion core: total channels, colour channels, and whether
/// the last channel is alpha.
#[derive(Clone, Copy)]
struct Shape {
    channels: usize,
    color: usize,
    alpha: bool,
}

/// Classifies `format`, rejecting the non-colour layouts.
fn shape_of(format: PixelFormat) -> Result<Shape> {
    let channels = format.channels();
    let (color, alpha) = match format.color_model() {
        ColorModel::Gray | ColorModel::Rgb | ColorModel::Cmyk => (channels, false),
        ColorModel::GrayAlpha | ColorModel::Rgba => (channels - 1, true),
        // Bilevel, Indexed, and any future non-continuous model.
        _ => return Err(CmmError::UnsupportedPixelFormat(format)),
    };
    Ok(Shape {
        channels,
        color,
        alpha,
    })
}

/// Validates the format pair against the transform's channel counts and returns the shapes.
fn check_formats(
    transform: &dyn Transform,
    src_format: PixelFormat,
    dst_format: PixelFormat,
) -> Result<(Shape, Shape)> {
    let src = shape_of(src_format)?;
    let dst = shape_of(dst_format)?;
    if src.color != usize::from(transform.input_channels()) {
        return Err(CmmError::ImageGeometry(
            "source format's colour channels differ from the transform's input channels",
        ));
    }
    if dst.color != usize::from(transform.output_channels()) {
        return Err(CmmError::ImageGeometry(
            "destination format's colour channels differ from the transform's output channels",
        ));
    }
    Ok((src, dst))
}

/// Rejects a `format` whose samples are not `S` wide, with `message` naming the side — the
/// check [`RawImage::new`] makes on the source, applied to a format no constructor vouched for.
fn check_width<S: Sample>(format: PixelFormat, message: &'static str) -> Result<()> {
    if format.bytes_per_sample() != size_of::<S>() {
        return Err(CmmError::ImageGeometry(message));
    }
    Ok(())
}

/// The reused colour-only scratch pair, sized for `min(pixels, CHUNK_PIXELS)` pixels.
fn scratch<S: Sample>(pixels: usize, src: Shape, dst: Shape) -> (Vec<S>, Vec<S>) {
    let chunk = pixels.min(CHUNK_PIXELS);
    (
        vec![S::default(); chunk * src.color],
        vec![S::default(); chunk * dst.color],
    )
}

/// Transforms `count` gathered pixels sitting in `cin` into `cout`.
fn run_chunk<S: ImageSample>(
    transform: &dyn Transform,
    cin: &[S],
    cout: &mut [S],
    count: usize,
    src: Shape,
    dst: Shape,
) -> Result<()> {
    S::run(
        transform,
        &cin[..count * src.color],
        &mut cout[..count * dst.color],
    )
}

/// Applies `transform` to the interleaved image `src`, writing `dst_format` pixels to `dst`.
///
/// `dst` must hold exactly as many `dst_format` pixels as `src` holds. `u8` and `u16` samples
/// run through the transform's entry point of that width ([`ImageSample`]); see the module
/// docs for the normalization, rounding, and alpha-passthrough rules. An
/// [`ImageRef`](gamut_core::ImageRef) is passed through its `as_raw` view.
///
/// # Errors
///
/// [`CmmError::UnsupportedPixelFormat`] for [`PixelFormat::Bilevel`]/
/// [`PixelFormat::Indexed8`]; [`CmmError::ImageGeometry`] when a format's colour channel
/// count differs from the transform's, or when `dst_format` does not store `S`-wide samples;
/// [`CmmError::BufferLength`] when `dst` does not hold the matching pixel count; plus any
/// error the transform itself returns.
///
/// # Example
///
/// ```
/// use gamut_cmm::{Pipeline, Stage, transform_interleaved};
/// use gamut_core::{Dimensions, PixelFormat, RawImage};
///
/// // A toy transform halving each RGB channel.
/// let halve = Stage::Matrix {
///     m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
///     offset: [0.0; 3],
/// };
/// let pipeline = Pipeline::new(3, 3, vec![halve])?;
///
/// let samples = [200u8, 100, 0, 255, 255, 255]; // two RGB pixels
/// let src = RawImage::new(&samples, PixelFormat::Rgb8, Dimensions::new(2, 1)?)?;
/// let mut dst = [0u8; 6];
/// transform_interleaved(&pipeline, src, PixelFormat::Rgb8, &mut dst)?;
/// assert_eq!(dst, [100, 50, 0, 128, 128, 128]); // 127.5 rounds half-up to 128
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn transform_interleaved<S: ImageSample>(
    transform: &dyn Transform,
    src: RawImage<'_, S>,
    dst_format: PixelFormat,
    dst: &mut [S],
) -> Result<()> {
    let src_format = src.format();
    let src = src.as_samples();
    let (src_shape, dst_shape) = check_formats(transform, src_format, dst_format)?;
    check_width::<S>(
        dst_format,
        "destination format's sample width differs from the buffer's",
    )?;
    // `RawImage` guarantees `src` is whole pixels of `src_format`.
    let pixels = src.len() / src_shape.channels;
    if dst.len() != pixels * dst_shape.channels {
        return Err(CmmError::BufferLength {
            channels: channel_count_u8(dst_shape),
            found: dst.len(),
        });
    }
    let (mut cin, mut cout) = scratch::<S>(pixels, src_shape, dst_shape);
    let src_chunks = src.chunks(CHUNK_PIXELS * src_shape.channels);
    let dst_chunks = dst.chunks_mut(CHUNK_PIXELS * dst_shape.channels);
    for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
        let count = src_chunk.len() / src_shape.channels;
        gather(src_chunk, src_shape, &mut cin);
        run_chunk(transform, &cin, &mut cout, count, src_shape, dst_shape)?;
        for (p, pixel) in dst_chunk.chunks_exact_mut(dst_shape.channels).enumerate() {
            for (c, sample) in pixel.iter_mut().take(dst_shape.color).enumerate() {
                *sample = cout[p * dst_shape.color + c];
            }
            if dst_shape.alpha {
                pixel[dst_shape.channels - 1] = if src_shape.alpha {
                    src_chunk[p * src_shape.channels + src_shape.channels - 1]
                } else {
                    S::MAX_VALUE
                };
            }
        }
    }
    Ok(())
}

/// Applies `transform` to planar buffers: one slice per channel, each holding exactly
/// `pixels` samples, in the format's channel order (alpha plane last for the alpha-bearing
/// formats). Chunks are gathered into the interleaved scratch, run through the same
/// conversion as [`transform_interleaved`], and scattered back — identical numeric results,
/// one extra copy (module docs).
///
/// # Errors
///
/// As [`transform_interleaved`], with [`CmmError::ImageGeometry`] additionally raised when
/// `src_format` does not store `S`-wide samples, when a side's plane count differs from its
/// format's channel count, or when any plane's length differs from `pixels`.
pub fn transform_planar<S: ImageSample>(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src_planes: &[&[S]],
    dst_format: PixelFormat,
    dst_planes: &mut [&mut [S]],
    pixels: usize,
) -> Result<()> {
    let (src_shape, dst_shape) = check_formats(transform, src_format, dst_format)?;
    check_width::<S>(
        src_format,
        "source format's sample width differs from the planes'",
    )?;
    check_width::<S>(
        dst_format,
        "destination format's sample width differs from the planes'",
    )?;
    if src_planes.len() != src_shape.channels {
        return Err(CmmError::ImageGeometry(
            "source plane count differs from the format's channel count",
        ));
    }
    if dst_planes.len() != dst_shape.channels {
        return Err(CmmError::ImageGeometry(
            "destination plane count differs from the format's channel count",
        ));
    }
    if src_planes.iter().any(|plane| plane.len() != pixels) {
        return Err(CmmError::ImageGeometry(
            "source plane length differs from the pixel count",
        ));
    }
    if dst_planes.iter().any(|plane| plane.len() != pixels) {
        return Err(CmmError::ImageGeometry(
            "destination plane length differs from the pixel count",
        ));
    }
    let (mut cin, mut cout) = scratch::<S>(pixels, src_shape, dst_shape);
    // Iterator-driven chunking: `step_by` owns the loop's progress, so no arithmetic
    // inside the body can stall it (a `while start < pixels { … start += count }` shape
    // hangs forever under mutation testing when the increment is mutated away).
    for start in (0..pixels).step_by(CHUNK_PIXELS) {
        let count = (pixels - start).min(CHUNK_PIXELS);
        for (c, plane) in src_planes.iter().take(src_shape.color).enumerate() {
            for (p, &sample) in plane[start..start + count].iter().enumerate() {
                cin[p * src_shape.color + c] = sample;
            }
        }
        run_chunk(transform, &cin, &mut cout, count, src_shape, dst_shape)?;
        for (c, plane) in dst_planes.iter_mut().enumerate() {
            for (p, sample) in plane[start..start + count].iter_mut().enumerate() {
                *sample = if c < dst_shape.color {
                    cout[p * dst_shape.color + c]
                } else if src_shape.alpha {
                    src_planes[src_shape.channels - 1][start + p]
                } else {
                    S::MAX_VALUE
                };
            }
        }
    }
    Ok(())
}

/// Applies `transform` to `image` **in place**: each pixel is replaced by its transformed
/// value.
///
/// The results are sample-for-sample those of [`transform_interleaved`] with `P`'s format on
/// both sides — the same normalization, rounding and alpha rules (an alpha channel is left as
/// it is) — but without a second buffer: the pixels pass through the same constant-size chunk
/// scratch (module docs), so a caller that owns a decoded raster converts it without doubling
/// its peak memory. The transform must therefore map `P`'s colour channels onto as many (an
/// `Rgb8` → `Cmyk8` conversion cannot be in place).
///
/// # Errors
///
/// [`CmmError::UnsupportedPixelFormat`] for a [`Bilevel`](gamut_core::Bilevel) or
/// [`Indexed8`](gamut_core::Indexed8) image and [`CmmError::ImageGeometry`] when `P`'s colour
/// channel count differs from the transform's input or output, both raised before any sample
/// is written. An error the transform itself returns stops the walk at that chunk, leaving the
/// earlier chunks transformed.
///
/// # Example
///
/// ```
/// use gamut_cmm::{Pipeline, Stage, transform_in_place};
/// use gamut_core::{Dimensions, ImageBuf, Rgba8};
///
/// let halve = Stage::Matrix {
///     m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
///     offset: [0.0; 3],
/// };
/// let pipeline = Pipeline::new(3, 3, vec![halve])?;
///
/// let pixels = vec![200u8, 100, 0, 9, 255, 255, 255, 77]; // two RGBA pixels
/// let mut image = ImageBuf::<Rgba8>::new(pixels, Dimensions::new(2, 1)?)?;
/// transform_in_place(&pipeline, &mut image)?;
/// assert_eq!(image.as_samples(), [100, 50, 0, 9, 128, 128, 128, 77]); // alpha untouched
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn transform_in_place<P: Pixel>(
    transform: &dyn Transform,
    image: &mut ImageBuf<P>,
) -> Result<()>
where
    P::Sample: ImageSample,
{
    let (shape, _) = check_formats(transform, P::FORMAT, P::FORMAT)?;
    // Always one full chunk of scratch (≤ 2 × 256 × 4 samples), not `min(pixels, chunk)`: the
    // trim is a capacity hint no output can observe, so computing it here only bred an
    // unkillable mutant. A tiny buffer pays at most 4 KiB.
    let (mut cin, mut cout) = scratch::<P::Sample>(CHUNK_PIXELS, shape, shape);
    // `ImageBuf` guarantees whole pixels, so every chunk is too.
    for chunk in image
        .as_mut_samples()
        .chunks_mut(CHUNK_PIXELS * shape.channels)
    {
        let count = chunk.len() / shape.channels;
        gather(chunk, shape, &mut cin);
        run_chunk(transform, &cin, &mut cout, count, shape, shape)?;
        for (pixel, colour) in chunk
            .chunks_exact_mut(shape.channels)
            .zip(cout.chunks_exact(shape.color))
        {
            pixel[..shape.color].copy_from_slice(colour);
        }
    }
    Ok(())
}

/// Copies the colour samples of the whole pixels in `chunk` into the colour-only scratch `cin`.
fn gather<S: Sample>(chunk: &[S], shape: Shape, cin: &mut [S]) {
    for (p, pixel) in chunk.chunks_exact(shape.channels).enumerate() {
        for (c, &sample) in pixel.iter().take(shape.color).enumerate() {
            cin[p * shape.color + c] = sample;
        }
    }
}

/// The `u8` channel count for [`CmmError::BufferLength`] (formats top out at 4 channels).
fn channel_count_u8(shape: Shape) -> u8 {
    u8::try_from(shape.channels).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, Rgb8, Rgba8, Rgba16};

    use super::*;
    use crate::pipeline::{Pipeline, Stage};
    use crate::transform::{encode_u8, encode_u16};

    /// `samples` as a one-row `format` image.
    fn raw<S: Sample>(samples: &[S], format: PixelFormat) -> RawImage<'_, S> {
        let pixels = u32::try_from(samples.len() / format.channels()).unwrap();
        RawImage::new(samples, format, Dimensions::new(pixels, 1).unwrap()).unwrap()
    }

    /// `samples` as an owned one-row `P` image.
    fn buf<P: Pixel>(samples: Vec<P::Sample>) -> ImageBuf<P> {
        let pixels = u32::try_from(samples.len() / P::CHANNELS).unwrap();
        ImageBuf::new(samples, Dimensions::new(pixels, 1).unwrap()).unwrap()
    }

    /// A 3→3 halving matrix transform.
    fn halve3() -> Pipeline {
        Pipeline::new(
            3,
            3,
            vec![Stage::Matrix {
                m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
                offset: [0.0; 3],
            }],
        )
        .unwrap()
    }

    /// A 3→4 transform (RGB → naive CMY0K-ish) with exact-dyadic arithmetic.
    fn rgb_to_cmyk() -> Pipeline {
        Pipeline::new(
            3,
            4,
            vec![Stage::MatrixN {
                rows: 4,
                cols: 3,
                m: vec![
                    -1.0, 0.0, 0.0, //
                    0.0, -1.0, 0.0, //
                    0.0, 0.0, -1.0, //
                    0.0, 0.0, 0.0,
                ],
                offset: vec![1.0, 1.0, 1.0, 0.25],
            }],
        )
        .unwrap()
    }

    /// A 1→1 identity.
    fn identity1() -> Pipeline {
        Pipeline::new(1, 1, vec![Stage::Identity { channels: 1 }]).unwrap()
    }

    #[test]
    fn interleaved_u8_matches_the_scalar_path_exactly() {
        // Every 8-bit gray code through a big enough buffer to cross the chunk boundary
        // (256 pixels = exactly one chunk; 300 forces two).
        let transform = halve3();
        let src: Vec<u8> = (0..300u32)
            .flat_map(|i| [u8::try_from(i % 256).unwrap(); 3])
            .collect();
        let mut dst = vec![0u8; src.len()];
        transform_interleaved(
            &transform,
            raw(&src, PixelFormat::Rgb8),
            PixelFormat::Rgb8,
            &mut dst,
        )
        .unwrap();
        for (s, d) in src.as_chunks::<3>().0.iter().zip(dst.as_chunks::<3>().0) {
            let mut want = [0.0; 3];
            let normalized: Vec<f64> = s.iter().map(|&v| f64::from(v) / 255.0).collect();
            transform.transform(&normalized, &mut want).unwrap();
            for ch in 0..3 {
                assert_eq!(d[ch], encode_u8(want[ch]), "pixel {s:?}");
            }
        }
    }

    #[test]
    fn interleaved_u16_matches_the_scalar_path_exactly() {
        let transform = halve3();
        let src: Vec<u16> = (0..777u32)
            .flat_map(|i| {
                let v = u16::try_from((i * 97) % 65536).unwrap();
                [v, v.wrapping_add(1000), 65535]
            })
            .collect();
        let mut dst = vec![0u16; src.len()];
        transform_interleaved(
            &transform,
            raw(&src, PixelFormat::Rgb16),
            PixelFormat::Rgb16,
            &mut dst,
        )
        .unwrap();
        for (s, d) in src.as_chunks::<3>().0.iter().zip(dst.as_chunks::<3>().0) {
            let normalized: Vec<f64> = s.iter().map(|&v| f64::from(v) / 65535.0).collect();
            let mut want = [0.0; 3];
            transform.transform(&normalized, &mut want).unwrap();
            for ch in 0..3 {
                assert_eq!(d[ch], encode_u16(want[ch]));
            }
        }
    }

    #[test]
    fn model_change_rgb8_to_cmyk8() {
        let transform = rgb_to_cmyk();
        let src = [255u8, 0, 128];
        let mut dst = [0u8; 4];
        transform_interleaved(
            &transform,
            raw(&src, PixelFormat::Rgb8),
            PixelFormat::Cmyk8,
            &mut dst,
        )
        .unwrap();
        // C = 1−1 = 0, M = 1−0 = 1, Y = 1 − 128/255, K = 0.25.
        assert_eq!(dst, [0, 255, encode_u8(1.0 - 128.0 / 255.0), 64]);
    }

    #[test]
    fn alpha_passes_through_untouched() {
        // Rgba8 → Rgba8: colour halved, alpha copied verbatim (never transformed).
        let transform = halve3();
        let src = [200u8, 100, 50, 7, 255, 255, 255, 250];
        let mut dst = [0u8; 8];
        transform_interleaved(
            &transform,
            raw(&src, PixelFormat::Rgba8),
            PixelFormat::Rgba8,
            &mut dst,
        )
        .unwrap();
        assert_eq!(dst, [100, 50, 25, 7, 128, 128, 128, 250]);
        // Rgba8 → Rgb8 drops the alpha; Rgb8 → Rgba8 fills opaque.
        let mut rgb = [0u8; 6];
        transform_interleaved(
            &transform,
            raw(&src, PixelFormat::Rgba8),
            PixelFormat::Rgb8,
            &mut rgb,
        )
        .unwrap();
        assert_eq!(rgb, [100, 50, 25, 128, 128, 128]);
        let mut rgba = [0u8; 8];
        transform_interleaved(
            &transform,
            raw(&rgb, PixelFormat::Rgb8),
            PixelFormat::Rgba8,
            &mut rgba,
        )
        .unwrap();
        assert_eq!(rgba, [50, 25, 13, 255, 64, 64, 64, 255]);
        // GrayAlpha16 keeps its 16-bit alpha too.
        let gray = identity1();
        let src16 = [40000u16, 1234, 65535, 60000];
        let mut dst16 = [0u16; 4];
        transform_interleaved(
            &gray,
            raw(&src16, PixelFormat::GrayAlpha16),
            PixelFormat::GrayAlpha16,
            &mut dst16,
        )
        .unwrap();
        assert_eq!(dst16, src16);
    }

    #[test]
    fn planar_equals_interleaved() {
        let transform = rgb_to_cmyk();
        let pixels = 300; // crosses the chunk boundary
        let r: Vec<u8> = (0..pixels)
            .map(|i| u8::try_from(i * 7 % 256).unwrap())
            .collect();
        let g: Vec<u8> = (0..pixels)
            .map(|i| u8::try_from(i * 13 % 256).unwrap())
            .collect();
        let b: Vec<u8> = (0..pixels)
            .map(|i| u8::try_from(i * 29 % 256).unwrap())
            .collect();
        let mut planes_out: Vec<Vec<u8>> = vec![vec![0; pixels]; 4];
        {
            let mut dst_planes: Vec<&mut [u8]> =
                planes_out.iter_mut().map(Vec::as_mut_slice).collect();
            transform_planar(
                &transform,
                PixelFormat::Rgb8,
                &[&r, &g, &b],
                PixelFormat::Cmyk8,
                &mut dst_planes,
                pixels,
            )
            .unwrap();
        }
        let interleaved_src: Vec<u8> = (0..pixels).flat_map(|i| [r[i], g[i], b[i]]).collect();
        let mut interleaved_dst = vec![0u8; pixels * 4];
        transform_interleaved(
            &transform,
            raw(&interleaved_src, PixelFormat::Rgb8),
            PixelFormat::Cmyk8,
            &mut interleaved_dst,
        )
        .unwrap();
        for p in 0..pixels {
            for c in 0..4 {
                assert_eq!(
                    planes_out[c][p],
                    interleaved_dst[p * 4 + c],
                    "px {p} ch {c}"
                );
            }
        }
    }

    #[test]
    fn planar_u16_alpha_planes() {
        // GrayAlpha16 planar → GrayAlpha16 planar: alpha plane copied; and a source
        // without alpha fills the destination alpha plane opaque.
        let gray = identity1();
        let luma = [1u16, 2, 3];
        let alpha = [10u16, 20, 30];
        let mut out = [vec![0u16; 3], vec![0u16; 3]];
        {
            let mut dst: Vec<&mut [u16]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            transform_planar(
                &gray,
                PixelFormat::GrayAlpha16,
                &[&luma, &alpha],
                PixelFormat::GrayAlpha16,
                &mut dst,
                3,
            )
            .unwrap();
        }
        assert_eq!(out[0], luma);
        assert_eq!(out[1], alpha);
        let mut out = [vec![0u16; 3], vec![0u16; 3]];
        {
            let mut dst: Vec<&mut [u16]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            transform_planar(
                &gray,
                PixelFormat::Gray16,
                &[&luma[..]],
                PixelFormat::GrayAlpha16,
                &mut dst,
                3,
            )
            .unwrap();
        }
        assert_eq!(out[0], luma);
        assert_eq!(out[1], [65535; 3]);
    }

    #[test]
    fn non_colour_formats_are_rejected() {
        let transform = identity1();
        let src = [0u8; 4];
        let mut dst = [0u8; 4];
        for format in [PixelFormat::Bilevel, PixelFormat::Indexed8] {
            let err =
                transform_interleaved(&transform, raw(&src, format), PixelFormat::Gray8, &mut dst)
                    .unwrap_err();
            assert!(
                matches!(err, CmmError::UnsupportedPixelFormat(f) if f == format),
                "{err}"
            );
            let err =
                transform_interleaved(&transform, raw(&src, PixelFormat::Gray8), format, &mut dst)
                    .unwrap_err();
            assert!(matches!(err, CmmError::UnsupportedPixelFormat(f) if f == format));
        }
    }

    #[test]
    fn geometry_errors_are_typed() {
        let transform = halve3(); // 3 → 3
        // Colour-channel mismatches, both sides.
        let err = transform_interleaved(
            &transform,
            raw(&[0u8; 3], PixelFormat::Gray8),
            PixelFormat::Rgb8,
            &mut [0; 9],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (source format's colour channels differ from the \
             transform's input channels)"
        );
        let err = transform_interleaved(
            &transform,
            raw(&[0u8; 3], PixelFormat::Rgb8),
            PixelFormat::Cmyk8,
            &mut [0; 4],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination format's colour channels differ from \
             the transform's output channels)"
        );
        // Pixel-count agreement reuses BufferLength (a partial source pixel cannot reach the
        // call: `RawImage::new` refuses it).
        let err = transform_interleaved(
            &transform,
            raw(&[0u8; 3], PixelFormat::Rgb8),
            PixelFormat::Rgb8,
            &mut [0; 4],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CmmError::BufferLength {
                channels: 3,
                found: 4
            }
        ));
        let err = transform_interleaved(
            &transform,
            raw(&[0u8; 6], PixelFormat::Rgb8),
            PixelFormat::Rgb8,
            &mut [0; 9],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CmmError::BufferLength {
                channels: 3,
                found: 9
            }
        ));
        // Planar-only geometry: plane counts and plane lengths.
        let r = [0u8; 3];
        let mut o1 = [0u8; 3];
        let mut o2 = [0u8; 3];
        let mut o3 = [0u8; 3];
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb8,
            &[&r, &r],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut o2, &mut o3],
            3,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (source plane count differs from the format's \
             channel count)"
        );
        let short = [0u8; 2];
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb8,
            &[&r, &r, &short],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut o2, &mut o3],
            3,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (source plane length differs from the pixel count)"
        );
        let mut two = [0u8; 3];
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb8,
            &[&r, &r, &r],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut two],
            3,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination plane count differs from the format's \
             channel count)"
        );
        let mut shorter = [0u8; 2];
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb8,
            &[&r, &r, &r],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut o2, &mut shorter],
            3,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination plane length differs from the pixel \
             count)"
        );
    }

    /// A 3→3 transform whose output channels each depend on all three inputs, so a pixel or
    /// channel misplaced by the in-place walk changes the bytes.
    fn mix3() -> Pipeline {
        Pipeline::new(
            3,
            3,
            vec![Stage::Matrix {
                m: [[0.5, 0.25, 0.125], [0.125, 0.5, 0.25], [0.25, 0.125, 0.5]],
                offset: [0.0; 3],
            }],
        )
        .unwrap()
    }

    /// Asserts the in-place walk over a 300-pixel `P` image (crossing the 256-pixel chunk
    /// boundary) leaves exactly what the interleaved walk writes.
    fn assert_in_place_equals_interleaved<P: Pixel>(src: Vec<P::Sample>)
    where
        P::Sample: ImageSample,
    {
        let transform = mix3();
        let mut want = vec![P::Sample::default(); src.len()];
        transform_interleaved(&transform, raw(&src, P::FORMAT), P::FORMAT, &mut want).unwrap();
        let mut image = buf::<P>(src);
        transform_in_place(&transform, &mut image).unwrap();
        assert_eq!(image.as_samples(), want, "{:?}", P::FORMAT);
    }

    #[test]
    fn in_place_u8_equals_interleaved_u8() {
        // Rgba8 pins that alpha stays put.
        let samples = |channels: usize| -> Vec<u8> {
            (0..300 * channels)
                .map(|i| u8::try_from((i * 37 + i / 7) % 256).unwrap())
                .collect()
        };
        assert_in_place_equals_interleaved::<Rgb8>(samples(3));
        assert_in_place_equals_interleaved::<Rgba8>(samples(4));
    }

    #[test]
    fn in_place_u16_equals_interleaved_u16() {
        let src: Vec<u16> = (0..300 * 4u32)
            .map(|i| u16::try_from((i * 9973 + i / 5) % 65536).unwrap())
            .collect();
        assert_in_place_equals_interleaved::<Rgba16>(src);
    }

    #[test]
    fn in_place_rejects_a_channel_changing_transform() {
        // RGB → CMYK has nowhere to put its fourth channel in an Rgb8 buffer.
        let mut image = buf::<Rgb8>(vec![0; 3]);
        let err = transform_in_place(&rgb_to_cmyk(), &mut image).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination format's colour channels differ from \
             the transform's output channels)"
        );
    }

    #[test]
    fn a_format_of_another_sample_width_is_rejected() {
        // `RawImage::new` holds the interleaved source to its width; every other format a
        // call names is checked here.
        let transform = halve3();
        let err = transform_interleaved(
            &transform,
            raw(&[0u8; 3], PixelFormat::Rgb8),
            PixelFormat::Rgb16,
            &mut [0u8; 3],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination format's sample width differs from the \
             buffer's)"
        );
        let plane = [0u8; 1];
        let (mut o1, mut o2, mut o3) = ([0u8; 1], [0u8; 1], [0u8; 1]);
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb16,
            &[&plane, &plane, &plane],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut o2, &mut o3],
            1,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (source format's sample width differs from the \
             planes')"
        );
        let err = transform_planar(
            &transform,
            PixelFormat::Rgb8,
            &[&plane, &plane, &plane],
            PixelFormat::Rgb16,
            &mut [&mut o1, &mut o2, &mut o3],
            1,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination format's sample width differs from the \
             planes')"
        );
    }

    #[test]
    fn empty_planes_are_a_valid_no_op() {
        // The one empty image left to express: `RawImage` and `ImageBuf` have non-zero
        // dimensions.
        let (mut o1, mut o2, mut o3) = ([0u8; 0], [0u8; 0], [0u8; 0]);
        transform_planar(
            &halve3(),
            PixelFormat::Rgb8,
            &[&[], &[], &[]],
            PixelFormat::Rgb8,
            &mut [&mut o1, &mut o2, &mut o3],
            0,
        )
        .unwrap();
    }
}
