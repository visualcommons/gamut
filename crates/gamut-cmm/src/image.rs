//! Typed pixel-buffer application: run any [`Transform`] over interleaved or planar
//! `u8`/`u16` image buffers tagged with [`gamut_core::PixelFormat`].
//!
//! # Semantics
//!
//! - **Colour channels** are normalized `v / 255` (8-bit) or `v / 65535` (16-bit), run
//!   through the transform, then re-encoded `round_half_up(v × scale)` clamped to the
//!   sample range — lcms2's `_cmsQuickSaturateWord` rounding (`floor(x + 0.5)`, saturating),
//!   at both widths.
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
//! Buffers are processed in fixed chunks of 256 pixels through **one reused
//! pair of `f64` scratch vectors** (≤ `2 × 256 × 16` samples ≈ 64 KiB), so peak extra
//! memory is constant in the image size while the transform still sees whole batches
//! rather than single pixels. Planar buffers gather each chunk from the per-channel planes
//! into the same interleaved scratch (and scatter back), trading one extra copy for reuse
//! of the identical conversion core.

use gamut_core::{ColorModel, PixelFormat};

use crate::error::{CmmError, Result};
use crate::transform::Transform;

/// Pixels processed per scratch-buffer refill (the module's space/time tradeoff).
const CHUNK_PIXELS: usize = 256;

/// The integer sample scalar of a buffer: its full-scale value and exact conversions.
trait Sample: Copy {
    /// The full-scale (and opaque-alpha) code value, e.g. `255` for `u8`.
    const OPAQUE: Self;
    /// The normalization divisor (`255.0` / `65535.0`).
    const FULL_SCALE: f64;
    /// Widen to `f64` (exact).
    fn to_f64(self) -> f64;
    /// Encode a normalized value: `floor(v × FULL_SCALE + 0.5)`, saturating to the sample
    /// range (lcms2's `_cmsQuickSaturateWord` rounding; NaN saturates to 0).
    fn encode(v: f64) -> Self;
}

impl Sample for u8 {
    const OPAQUE: Self = u8::MAX;
    const FULL_SCALE: f64 = 255.0;
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn encode(v: f64) -> Self {
        // `as` saturates (NaN → 0), so the clamp only needs the rounding to be done first.
        #[expect(clippy::cast_possible_truncation, reason = "clamped to the u8 range")]
        #[expect(clippy::cast_sign_loss, reason = "clamped to be non-negative")]
        {
            (v * Self::FULL_SCALE + 0.5).floor().clamp(0.0, 255.0) as u8
        }
    }
}

impl Sample for u16 {
    const OPAQUE: Self = u16::MAX;
    const FULL_SCALE: f64 = 65535.0;
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn encode(v: f64) -> Self {
        #[expect(clippy::cast_possible_truncation, reason = "clamped to the u16 range")]
        #[expect(clippy::cast_sign_loss, reason = "clamped to be non-negative")]
        {
            (v * Self::FULL_SCALE + 0.5).floor().clamp(0.0, 65535.0) as u16
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

/// The reused scratch pair, sized for `min(pixels, CHUNK_PIXELS)` pixels.
fn scratch(pixels: usize, src: Shape, dst: Shape) -> (Vec<f64>, Vec<f64>) {
    let chunk = pixels.min(CHUNK_PIXELS);
    (vec![0.0; chunk * src.color], vec![0.0; chunk * dst.color])
}

/// Transforms `count` gathered pixels sitting in `fin` into `fout`.
fn run_chunk(
    transform: &dyn Transform,
    fin: &[f64],
    fout: &mut [f64],
    count: usize,
    src: Shape,
    dst: Shape,
) -> Result<()> {
    transform.transform(&fin[..count * src.color], &mut fout[..count * dst.color])
}

/// Applies `transform` to an interleaved buffer (both sides `u8`).
///
/// `src` holds whole `src_format` pixels; `dst` must hold exactly the same number of
/// `dst_format` pixels. See the module docs for the normalization, rounding, and
/// alpha-passthrough rules.
///
/// # Errors
///
/// [`CmmError::UnsupportedPixelFormat`] for [`PixelFormat::Bilevel`]/
/// [`PixelFormat::Indexed8`]; [`CmmError::ImageGeometry`] when a format's colour channel
/// count differs from the transform's; [`CmmError::BufferLength`] when `src` is not a whole
/// number of pixels or `dst` does not hold the matching pixel count; plus any error the
/// transform itself returns.
///
/// # Example
///
/// ```
/// use gamut_cmm::{Pipeline, Stage, transform_interleaved_u8};
/// use gamut_core::PixelFormat;
///
/// // A toy transform halving each RGB channel.
/// let halve = Stage::Matrix {
///     m: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
///     offset: [0.0; 3],
/// };
/// let pipeline = Pipeline::new(3, 3, vec![halve])?;
///
/// let src = [200u8, 100, 0, 255, 255, 255]; // two RGB pixels
/// let mut dst = [0u8; 6];
/// transform_interleaved_u8(&pipeline, PixelFormat::Rgb8, &src, PixelFormat::Rgb8, &mut dst)?;
/// assert_eq!(dst, [100, 50, 0, 128, 128, 128]); // 127.5 rounds half-up to 128
/// # Ok::<(), gamut_cmm::CmmError>(())
/// ```
pub fn transform_interleaved_u8(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src: &[u8],
    dst_format: PixelFormat,
    dst: &mut [u8],
) -> Result<()> {
    interleaved(transform, src_format, src, dst_format, dst)
}

/// [`transform_interleaved_u8`] for 16-bit samples (normalization by 65535; identical
/// geometry and alpha rules).
///
/// # Errors
///
/// As [`transform_interleaved_u8`].
pub fn transform_interleaved_u16(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src: &[u16],
    dst_format: PixelFormat,
    dst: &mut [u16],
) -> Result<()> {
    interleaved(transform, src_format, src, dst_format, dst)
}

/// Applies `transform` to planar buffers (both sides `u8`): one slice per channel, each
/// holding exactly `pixels` samples, in the format's channel order (alpha plane last for
/// the alpha-bearing formats). Chunks are gathered into the interleaved scratch, run
/// through the same core as [`transform_interleaved_u8`], and scattered back — identical
/// numeric results, one extra copy (module docs).
///
/// # Errors
///
/// As [`transform_interleaved_u8`], with [`CmmError::ImageGeometry`] additionally raised
/// when a side's plane count differs from its format's channel count or any plane's length
/// differs from `pixels`.
pub fn transform_planar_u8(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src_planes: &[&[u8]],
    dst_format: PixelFormat,
    dst_planes: &mut [&mut [u8]],
    pixels: usize,
) -> Result<()> {
    planar(
        transform, src_format, src_planes, dst_format, dst_planes, pixels,
    )
}

/// [`transform_planar_u8`] for 16-bit samples.
///
/// # Errors
///
/// As [`transform_planar_u8`].
pub fn transform_planar_u16(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src_planes: &[&[u16]],
    dst_format: PixelFormat,
    dst_planes: &mut [&mut [u16]],
    pixels: usize,
) -> Result<()> {
    planar(
        transform, src_format, src_planes, dst_format, dst_planes, pixels,
    )
}

/// The shared interleaved core.
fn interleaved<S: Sample>(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src: &[S],
    dst_format: PixelFormat,
    dst: &mut [S],
) -> Result<()> {
    let (src_shape, dst_shape) = check_formats(transform, src_format, dst_format)?;
    if !src.len().is_multiple_of(src_shape.channels) {
        return Err(CmmError::BufferLength {
            channels: channel_count_u8(src_shape),
            found: src.len(),
        });
    }
    let pixels = src.len() / src_shape.channels;
    if dst.len() != pixels * dst_shape.channels {
        return Err(CmmError::BufferLength {
            channels: channel_count_u8(dst_shape),
            found: dst.len(),
        });
    }
    let (mut fin, mut fout) = scratch(pixels, src_shape, dst_shape);
    let src_chunks = src.chunks(CHUNK_PIXELS * src_shape.channels);
    let dst_chunks = dst.chunks_mut(CHUNK_PIXELS * dst_shape.channels);
    for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
        let count = src_chunk.len() / src_shape.channels;
        for (p, pixel) in src_chunk.chunks_exact(src_shape.channels).enumerate() {
            for (c, sample) in pixel.iter().take(src_shape.color).enumerate() {
                fin[p * src_shape.color + c] = sample.to_f64() / S::FULL_SCALE;
            }
        }
        run_chunk(transform, &fin, &mut fout, count, src_shape, dst_shape)?;
        for (p, pixel) in dst_chunk.chunks_exact_mut(dst_shape.channels).enumerate() {
            for (c, sample) in pixel.iter_mut().take(dst_shape.color).enumerate() {
                *sample = S::encode(fout[p * dst_shape.color + c]);
            }
            if dst_shape.alpha {
                pixel[dst_shape.channels - 1] = if src_shape.alpha {
                    src_chunk[p * src_shape.channels + src_shape.channels - 1]
                } else {
                    S::OPAQUE
                };
            }
        }
    }
    Ok(())
}

/// The shared planar core: per-chunk gather → interleaved transform → scatter.
fn planar<S: Sample>(
    transform: &dyn Transform,
    src_format: PixelFormat,
    src_planes: &[&[S]],
    dst_format: PixelFormat,
    dst_planes: &mut [&mut [S]],
    pixels: usize,
) -> Result<()> {
    let (src_shape, dst_shape) = check_formats(transform, src_format, dst_format)?;
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
    let (mut fin, mut fout) = scratch(pixels, src_shape, dst_shape);
    // Iterator-driven chunking: `step_by` owns the loop's progress, so no arithmetic
    // inside the body can stall it (a `while start < pixels { … start += count }` shape
    // hangs forever under mutation testing when the increment is mutated away).
    for start in (0..pixels).step_by(CHUNK_PIXELS) {
        let count = (pixels - start).min(CHUNK_PIXELS);
        for (c, plane) in src_planes.iter().take(src_shape.color).enumerate() {
            for (p, sample) in plane[start..start + count].iter().enumerate() {
                fin[p * src_shape.color + c] = sample.to_f64() / S::FULL_SCALE;
            }
        }
        run_chunk(transform, &fin, &mut fout, count, src_shape, dst_shape)?;
        for (c, plane) in dst_planes.iter_mut().enumerate() {
            for (p, sample) in plane[start..start + count].iter_mut().enumerate() {
                *sample = if c < dst_shape.color {
                    S::encode(fout[p * dst_shape.color + c])
                } else if src_shape.alpha {
                    src_planes[src_shape.channels - 1][start + p]
                } else {
                    S::OPAQUE
                };
            }
        }
    }
    Ok(())
}

/// The `u8` channel count for [`CmmError::BufferLength`] (formats top out at 4 channels).
fn channel_count_u8(shape: Shape) -> u8 {
    u8::try_from(shape.channels).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{Pipeline, Stage};

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
    fn rounding_is_half_up_with_saturation() {
        assert_eq!(u8::encode(0.5), 128, "127.5 rounds up");
        assert_eq!(u8::encode(1.0), 255);
        assert_eq!(u8::encode(1.5), 255, "saturates high");
        assert_eq!(u8::encode(-0.2), 0, "saturates low");
        assert_eq!(u8::encode(f64::NAN), 0, "NaN saturates to 0");
        assert_eq!(u16::encode(0.5), 32768, "32767.5 rounds up");
        assert_eq!(u16::encode(1.0), 65535);
        assert_eq!(u16::encode(2.0), 65535);
        assert_eq!(u16::encode(-1.0), 0);
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
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &src,
            PixelFormat::Rgb8,
            &mut dst,
        )
        .unwrap();
        for (s, d) in src.chunks_exact(3).zip(dst.chunks_exact(3)) {
            let mut want = [0.0; 3];
            let normalized: Vec<f64> = s.iter().map(|&v| f64::from(v) / 255.0).collect();
            transform.transform(&normalized, &mut want).unwrap();
            for ch in 0..3 {
                assert_eq!(d[ch], u8::encode(want[ch]), "pixel {s:?}");
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
        transform_interleaved_u16(
            &transform,
            PixelFormat::Rgb16,
            &src,
            PixelFormat::Rgb16,
            &mut dst,
        )
        .unwrap();
        for (s, d) in src.chunks_exact(3).zip(dst.chunks_exact(3)) {
            let normalized: Vec<f64> = s.iter().map(|&v| f64::from(v) / 65535.0).collect();
            let mut want = [0.0; 3];
            transform.transform(&normalized, &mut want).unwrap();
            for ch in 0..3 {
                assert_eq!(d[ch], u16::encode(want[ch]));
            }
        }
    }

    #[test]
    fn model_change_rgb8_to_cmyk8() {
        let transform = rgb_to_cmyk();
        let src = [255u8, 0, 128];
        let mut dst = [0u8; 4];
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &src,
            PixelFormat::Cmyk8,
            &mut dst,
        )
        .unwrap();
        // C = 1−1 = 0, M = 1−0 = 1, Y = 1 − 128/255, K = 0.25.
        assert_eq!(dst, [0, 255, u8::encode(1.0 - 128.0 / 255.0), 64]);
    }

    #[test]
    fn alpha_passes_through_untouched() {
        // Rgba8 → Rgba8: colour halved, alpha copied verbatim (never transformed).
        let transform = halve3();
        let src = [200u8, 100, 50, 7, 255, 255, 255, 250];
        let mut dst = [0u8; 8];
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgba8,
            &src,
            PixelFormat::Rgba8,
            &mut dst,
        )
        .unwrap();
        assert_eq!(dst, [100, 50, 25, 7, 128, 128, 128, 250]);
        // Rgba8 → Rgb8 drops the alpha; Rgb8 → Rgba8 fills opaque.
        let mut rgb = [0u8; 6];
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgba8,
            &src,
            PixelFormat::Rgb8,
            &mut rgb,
        )
        .unwrap();
        assert_eq!(rgb, [100, 50, 25, 128, 128, 128]);
        let mut rgba = [0u8; 8];
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &rgb,
            PixelFormat::Rgba8,
            &mut rgba,
        )
        .unwrap();
        assert_eq!(rgba, [50, 25, 13, 255, 64, 64, 64, 255]);
        // GrayAlpha16 keeps its 16-bit alpha too.
        let gray = identity1();
        let src16 = [40000u16, 1234, 65535, 60000];
        let mut dst16 = [0u16; 4];
        transform_interleaved_u16(
            &gray,
            PixelFormat::GrayAlpha16,
            &src16,
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
            transform_planar_u8(
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
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &interleaved_src,
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
            transform_planar_u16(
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
            transform_planar_u16(
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
                transform_interleaved_u8(&transform, format, &src, PixelFormat::Gray8, &mut dst)
                    .unwrap_err();
            assert!(
                matches!(err, CmmError::UnsupportedPixelFormat(f) if f == format),
                "{err}"
            );
            let err =
                transform_interleaved_u8(&transform, PixelFormat::Gray8, &src, format, &mut dst)
                    .unwrap_err();
            assert!(matches!(err, CmmError::UnsupportedPixelFormat(f) if f == format));
        }
    }

    #[test]
    fn geometry_errors_are_typed() {
        let transform = halve3(); // 3 → 3
        // Colour-channel mismatches, both sides.
        let err = transform_interleaved_u8(
            &transform,
            PixelFormat::Gray8,
            &[0; 3],
            PixelFormat::Rgb8,
            &mut [0; 9],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (source format's colour channels differ from the \
             transform's input channels)"
        );
        let err = transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &[0; 3],
            PixelFormat::Cmyk8,
            &mut [0; 4],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: image geometry mismatch (destination format's colour channels differ from \
             the transform's output channels)"
        );
        // Length divisibility and pixel-count agreement reuse BufferLength.
        let err = transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &[0; 4],
            PixelFormat::Rgb8,
            &mut [0; 3],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CmmError::BufferLength {
                channels: 3,
                found: 4
            }
        ));
        let err = transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &[0; 6],
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
        let err = transform_planar_u8(
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
        let err = transform_planar_u8(
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
        let err = transform_planar_u8(
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
        let err = transform_planar_u8(
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

    #[test]
    fn empty_buffers_are_a_valid_no_op() {
        let transform = halve3();
        let mut dst: [u8; 0] = [];
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            &[],
            PixelFormat::Rgb8,
            &mut dst,
        )
        .unwrap();
    }
}
