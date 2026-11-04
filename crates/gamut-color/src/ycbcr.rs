//! YCbCr ↔ RGB conversion: a bit-depth- and matrix-generic presentation layer, and the
//! libwebp-exact 8-bit BT.601 layer VP8 (WebP lossy) requires.
//!
//! # Two layers, deliberately not one
//!
//! - [`YcbcrMatrix`] / [`RgbToYcbcr`] — the **H.273** layer, one type per direction. They apply the
//!   normative ITU-T H.273 §8.3 non-constant-luminance de-matrixing and matrixing for BT.709 /
//!   BT.601 / BT.470 B,G / BT.2020 NCL, in either [`ColorRange`], at every modeled [`BitDepth`]
//!   (8/10/12/16). [`YcbcrMatrix`] is what a still-image container reaches for when it has a `colr`
//!   box and a decoded frame — 10-bit HDR HEIC/AVIF included; [`RgbToYcbcr`] is what an encoder
//!   reaches for to code a lossy still in YCbCr.
//! - [`rgb_to_ycbcr`] / [`ycbcr_to_rgb`] / [`Yuv420`] — the **VP8** layer, 8-bit BT.601 only. Its
//!   limited-range inverse is a bit-exact port of libwebp's `VP8YUVToR/G/B` (`src/dsp/yuv.h`),
//!   pinned per pixel against libwebp by gamut-webp's oracle tests.
//!
//! The VP8 layer is **not** a special case of the H.273 one, and the H.273 one is not built on it.
//! Both are correct BT.601, and they differ only in how they round: libwebp's Q6 `MultHi`
//! truncates intermediates and the full-range arm's Q16 constants are truncated transcriptions,
//! while [`YcbcrMatrix`] / [`RgbToYcbcr`] derive exactly and round once at Q20. They therefore
//! agree on the great majority of 8-bit triples and differ by **at most 1 LSB** on the rest. Use
//! [`rgb_to_ycbcr`] / [`ycbcr_to_rgb`] when you must match libwebp byte for byte (i.e. WebP); use
//! the H.273 pair everywhere else.
//!
//! # The VP8 layer
//!
//! VP8 codes images as YCbCr 4:2:0 (`color_space = 0`, RFC 6386 §9.2; the WebP container references
//! BT.601). The *signal range* is selected with [`ColorRange`] — the same CICP range flag used in
//! `colr` / AV1 headers, so callers carry one range type end to end:
//!
//! - [`ColorRange::Limited`] — studio swing (Y ∈ `16..=235`, chroma ∈ `16..=240`). **This is what
//!   the WebP ecosystem uses**: libwebp's `cwebp`/`dwebp`, browsers, and every standard decoder
//!   assume limited range, so WebP files must be encoded this way to render with correct colors. The
//!   limited-range path reproduces libwebp's exact integer math (`src/dsp/yuv.h` `VP8RGBToY/U/V` and
//!   `VP8YUVToR/G/B`), so gamut and libwebp agree per pixel.
//! - [`ColorRange::Full`] — full / "JFIF" swing (Y and chroma over the whole `0..=255` range), the
//!   JPEG convention. Kept for callers that genuinely want full-range BT.601; it is **not** what a
//!   WebP file should carry.
//!
//! Chroma is box-subsampled (2×2 average) on the way down. On the way up, [`Yuv420::to_rgb8`] takes
//! a [`ChromaUpsampling`]. [`ChromaUpsampling::Interpolated`] is libwebp's fancy upsampler, ported
//! exactly ([`crate::upsample::libwebp`]), so RGB from a WebP decode is byte-identical to
//! `dwebp`'s default output. [`ChromaUpsampling::Nearest`] replicates each chroma sample across its
//! 2×2 luma block (`dwebp -nofancy`). Sharp-YUV downsampling on the encode side is not implemented.
//! This conversion is deliberately *not* on the VP8 codec's bit-exact path (the codec operates on
//! YCbCr planes directly); it backs the public RGB API.

use gamut_core::luminance::BT601_LUMA_WEIGHTS;
use gamut_core::{Dimensions, Error, Result};

use crate::cicp::{ColorRange, MatrixCoefficients};
use crate::format::BitDepth;
use crate::upsample::{ChromaUpsampling, libwebp, replicate, taps};
use crate::{clip_pixel, clip_pixel8};

/// Fixed-point fractional bits for the conversion coefficients.
///
/// The same scale as [`LUMA_FIX`], which is what lets the full-range luma row below come straight
/// from `gamut_core::luminance` rather than being restated here (pinned by a test).
const FIX: i32 = 16;

/// The full-range BT.601 luma row, widened once from the workspace's single authoritative
/// definition so the per-pixel expression stays integer-typed.
///
/// Only the *luma* row is shared: the chroma rows below, and every limited-range coefficient, are
/// libwebp's own studio-swing values with no counterpart in `gamut_core`.
const LUMA_R: i32 = BT601_LUMA_WEIGHTS[0] as i32;
/// Green's full-range BT.601 luma coefficient. See [`LUMA_R`].
const LUMA_G: i32 = BT601_LUMA_WEIGHTS[1] as i32;
/// Blue's full-range BT.601 luma coefficient. See [`LUMA_R`].
const LUMA_B: i32 = BT601_LUMA_WEIGHTS[2] as i32;
/// Rounding addend (`0.5` in the fixed-point scale).
const HALF: i32 = 1 << (FIX - 1);
/// The chroma offset (`128`) pre-scaled to the fixed-point domain, with rounding folded in.
const CHROMA_BIAS: i32 = (128 << FIX) + HALF;
/// The limited-range luma offset (`16`) pre-scaled to the fixed-point domain.
const LUMA_BIAS_LIMITED: i32 = 16 << FIX;
/// Fractional bits of libwebp's YUV→RGB inverse (`YUV_FIX2` in `src/dsp/yuv.h`).
const FIX2: i32 = 6;
/// In-range mask for libwebp's `VP8Clip8` fast path (`YUV_MASK2 = (256 << YUV_FIX2) - 1`).
const MASK2: i32 = (256 << FIX2) - 1;

/// libwebp's `MultHi` (`src/dsp/yuv.h`): the high bits of a fixed-point product, `(v * coeff) >> 8`.
fn mult_hi(v: i32, coeff: i32) -> i32 {
    (v * coeff) >> 8
}

/// libwebp's `VP8Clip8` (`src/dsp/yuv.h`): a `>> FIX2` shift for in-range values, else a hard clamp.
fn vp8_clip8(v: i32) -> u8 {
    if v & !MASK2 == 0 {
        (v >> FIX2) as u8
    } else if v < 0 {
        0
    } else {
        255
    }
}

/// Converts one RGB triple to BT.601 YCbCr in the given [`ColorRange`] (each component `0..=255`).
///
/// # Examples
///
/// ```
/// use gamut_color::{rgb_to_ycbcr, ColorRange};
/// // Limited-range BT.601 (what WebP uses): black → luma 16, neutral chroma 128.
/// assert_eq!(rgb_to_ycbcr(0, 0, 0, ColorRange::Limited), (16, 128, 128));
/// ```
#[must_use]
pub fn rgb_to_ycbcr(r: u8, g: u8, b: u8, range: ColorRange) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    match range {
        ColorRange::Full => {
            let y = (LUMA_R * r + LUMA_G * g + LUMA_B * b + HALF) >> FIX;
            let cb = (-11059 * r - 21709 * g + 32768 * b + CHROMA_BIAS) >> FIX;
            let cr = (32768 * r - 27439 * g - 5329 * b + CHROMA_BIAS) >> FIX;
            (clip_pixel8(y), clip_pixel8(cb), clip_pixel8(cr))
        }
        // libwebp's per-pixel coefficients (src/dsp/yuv.h `VP8RGBToY/U/V`): studio swing, +16 luma
        // offset; chroma uses the same `(128 << FIX) + HALF` bias as the full-range path.
        ColorRange::Limited => {
            let y = (16839 * r + 33059 * g + 6420 * b + LUMA_BIAS_LIMITED + HALF) >> FIX;
            let cb = (-9719 * r - 19081 * g + 28800 * b + CHROMA_BIAS) >> FIX;
            let cr = (28800 * r - 24116 * g - 4684 * b + CHROMA_BIAS) >> FIX;
            (clip_pixel8(y), clip_pixel8(cb), clip_pixel8(cr))
        }
    }
}

/// Converts one BT.601 YCbCr triple in the given [`ColorRange`] back to RGB (each `0..=255`).
#[must_use]
pub fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, range: ColorRange) -> (u8, u8, u8) {
    match range {
        ColorRange::Full => {
            let y = i32::from(y);
            let cb = i32::from(cb) - 128;
            let cr = i32::from(cr) - 128;
            let r = y + ((91881 * cr + HALF) >> FIX);
            let g = y + ((-22554 * cb - 46802 * cr + HALF) >> FIX);
            let b = y + ((116130 * cb + HALF) >> FIX);
            (clip_pixel8(r), clip_pixel8(g), clip_pixel8(b))
        }
        // libwebp's exact per-pixel inverse (src/dsp/yuv.h `VP8YUVToR/G/B`): the studio-swing offsets
        // are folded into the additive constants, so the raw 0..=255 samples feed straight in.
        ColorRange::Limited => {
            let (y, cb, cr) = (i32::from(y), i32::from(cb), i32::from(cr));
            let yy = mult_hi(y, 19077);
            let r = vp8_clip8(yy + mult_hi(cr, 26149) - 14234);
            let g = vp8_clip8(yy - mult_hi(cb, 6419) - mult_hi(cr, 13320) + 8708);
            let b = vp8_clip8(yy + mult_hi(cb, 33050) - 17685);
            (r, g, b)
        }
    }
}

/// A BT.601 YCbCr image in 4:2:0 layout: a `width × height` luma plane and two
/// `chroma_width × chroma_height` chroma planes, all row-major 8-bit, where the chroma dimensions are
/// `ceil(width / 2)` and `ceil(height / 2)`. The signal range is a property of the conversion
/// ([`ColorRange`]), not of the stored planes.
#[derive(Debug, Clone)]
pub struct Yuv420 {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Yuv420 {
    /// Chroma plane width, `ceil(width / 2)`.
    #[must_use]
    pub fn chroma_width(width: u32) -> u32 {
        width.div_ceil(2)
    }

    /// Chroma plane height, `ceil(height / 2)`.
    #[must_use]
    pub fn chroma_height(height: u32) -> u32 {
        height.div_ceil(2)
    }

    /// Builds a buffer from existing planes (e.g. a decoder's output), validating their lengths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if any plane length does not match the dimensions, or if
    /// the luma sample count overflows `usize`.
    pub fn new(width: u32, height: u32, y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> Result<Self> {
        let luma = Dimensions { width, height }.num_pixels().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "image dimensions overflow usize")
        })?;
        // Cannot overflow: each chroma plane has at most as many samples as the luma plane
        // (`ceil(d / 2) <= d` for d >= 1, and 0 for d == 0), and `luma` just fit `usize`.
        let chroma = Self::chroma_width(width) as usize * Self::chroma_height(height) as usize;
        if y.len() != luma || u.len() != chroma || v.len() != chroma {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "YUV plane length does not match dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            y,
            u,
            v,
        })
    }

    /// Converts an interleaved 8-bit RGB image to YCbCr 4:2:0 in the given [`ColorRange`],
    /// box-averaging each 2×2 block of chroma (partial edge blocks average only the pixels that exist).
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::{Yuv420, ColorRange};
    /// let rgb = vec![128u8; 4 * 4 * 3]; // 4×4 flat gray
    /// let yuv = Yuv420::from_rgb8(&rgb, 4, 4, ColorRange::Limited).expect("valid length");
    /// assert_eq!(yuv.y().len(), 16); // full-resolution luma
    /// assert_eq!(yuv.u().len(), 4); // 2×2-subsampled chroma
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `rgb.len() != width * height * 3`, if that product
    /// overflows `usize`, or if either dimension is 0.
    pub fn from_rgb8(rgb: &[u8], width: u32, height: u32, range: ColorRange) -> Result<Self> {
        let samples = Dimensions::new(width, height)?
            .sample_count(3)
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "image dimensions overflow usize")
            })?;
        if rgb.len() != samples {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "rgb buffer length does not match dimensions",
            ));
        }
        let (w, h) = (width as usize, height as usize);
        // Full-resolution luma, plus full-resolution chroma we then average down.
        let mut y = vec![0u8; w * h];
        let mut cb_full = vec![0u8; w * h];
        let mut cr_full = vec![0u8; w * h];
        for i in 0..w * h {
            let (yy, cb, cr) = rgb_to_ycbcr(rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2], range);
            y[i] = yy;
            cb_full[i] = cb;
            cr_full[i] = cr;
        }

        let cw = Self::chroma_width(width) as usize;
        let ch = Self::chroma_height(height) as usize;
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut su, mut sv, mut count) = (0u32, 0u32, 0u32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                        if px < w && py < h {
                            su += u32::from(cb_full[py * w + px]);
                            sv += u32::from(cr_full[py * w + px]);
                            count += 1;
                        }
                    }
                }
                u[cy * cw + cx] = ((su + count / 2) / count) as u8;
                v[cy * cw + cx] = ((sv + count / 2) / count) as u8;
            }
        }
        Ok(Self {
            width,
            height,
            y,
            u,
            v,
        })
    }

    /// Converts back to an interleaved 8-bit RGB image in the given [`ColorRange`], reconstructing
    /// full-resolution chroma with `upsampling`.
    ///
    /// [`ChromaUpsampling::Interpolated`] is libwebp's fancy upsampler
    /// ([`crate::upsample::libwebp::fancy_row`]), which together with the libwebp-exact
    /// [`ycbcr_to_rgb`] makes the output byte-identical to libwebp's default RGB decode.
    /// [`ChromaUpsampling::Nearest`] replicates each chroma sample across its 2×2 luma block.
    #[must_use]
    pub fn to_rgb8(&self, range: ColorRange, upsampling: ChromaUpsampling) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let cw = Self::chroma_width(self.width) as usize;
        let ch = Self::chroma_height(self.height) as usize;
        let mut out = vec![0u8; w * h * 3];
        if w == 0 {
            // `new` admits a zero-width buffer; it has no rows to convert.
            return out;
        }
        let (mut u_row, mut v_row) = (vec![0u8; w], vec![0u8; w]);
        for (py, line) in out.chunks_exact_mut(w * 3).enumerate() {
            let (near, far) = taps(py, ch);
            let (near, far) = (near * cw..(near + 1) * cw, far * cw..(far + 1) * cw);
            for (plane, row) in [(&self.u, &mut u_row), (&self.v, &mut v_row)] {
                match upsampling {
                    ChromaUpsampling::Nearest => replicate(&plane[near.clone()], row),
                    ChromaUpsampling::Interpolated => {
                        libwebp::fancy_row(&plane[near.clone()], &plane[far.clone()], row);
                    }
                }
            }
            let luma = &self.y[py * w..(py + 1) * w];
            for (((px, &y), &u), &v) in line
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(luma)
                .zip(&u_row)
                .zip(&v_row)
            {
                let (r, g, b) = ycbcr_to_rgb(y, u, v, range);
                *px = [r, g, b];
            }
        }
        out
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The full-resolution luma plane (`width * height` samples, row-major).
    #[must_use]
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// The subsampled Cb (U) plane.
    #[must_use]
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// The subsampled Cr (V) plane.
    #[must_use]
    pub fn v(&self) -> &[u8] {
        &self.v
    }
}

// ---- generic H.273 presentation layer --------------------------------------------------------

/// Fractional bits of the generic de-matrixing coefficients.
///
/// Wider than the VP8 layer's [`FIX`]: at 16-bit inputs a Q16 coefficient's quantization error
/// reaches about one output LSB, while Q20 puts the overwhelming majority of samples on the
/// exactly-rounded value. Q24 would gain a little more but leaves no headroom for a future
/// `i32`-lane vector path.
const MATRIX_FIX: u32 = 20;
/// Extra fractional bits of a chroma sample given in sixteenths of a code value
/// ([`YcbcrMatrix::to_rgb_sixteenths`]).
const SIXTEENTHS_FIX: u32 = 4;
/// Denominator of the published `Kr`/`Kb` luma weights (ITU-T H.273 Table 4), which are exact
/// four-decimal values — so the whole coefficient derivation is exact integer arithmetic.
const K_DEN: i128 = 10_000;

/// Rounds `num / den` to the nearest integer, halves away from zero. `den` must be positive.
fn round_div(num: i128, den: i128) -> i128 {
    debug_assert!(den > 0, "round_div denominator must be positive");
    if num >= 0 {
        (2 * num + den) / (2 * den)
    } else {
        -((-2 * num + den) / (2 * den))
    }
}

/// Rounds a [`MATRIX_FIX`]-scaled accumulator to an integer sample and saturates it to `bit_depth`
/// (the AV1 `Clip1` of [`clip_pixel`]).
fn round_clip(acc: i64, bit_depth: u32) -> u16 {
    round_clip_at(acc, MATRIX_FIX, bit_depth)
}

/// [`round_clip`] for an accumulator with `frac` fractional bits: rounds half up, then saturates.
fn round_clip_at(acc: i64, frac: u32, bit_depth: u32) -> u16 {
    // Over every derived coefficient set and every `u16` input triple the shifted value stays well
    // inside 19 bits, so narrowing to `i32` for `clip_pixel` cannot truncate.
    let v = (acc + (1 << (frac - 1))) >> frac;
    debug_assert!(i32::try_from(v).is_ok(), "YCbCr accumulator exceeds i32");
    clip_pixel(v as i32, bit_depth)
}

/// A precomputed non-constant-luminance YCbCr → RGB de-matrixing (ITU-T H.273 §8.3) for one
/// (matrix coefficients, range, bit depth) triple.
///
/// Deriving the coefficients costs a handful of divisions; [`to_rgb`](Self::to_rgb) costs three
/// multiplies and a shift. **Build one per image or per plane, never inside a pixel loop.**
///
/// Samples are `u16` at every depth, with a `bit_depth`-bit plane in the low bits — so an 8-bit
/// caller widens with `u16::from(..)`, and the outputs land in `0..=(1 << bit_depth) - 1`. Inputs
/// outside the nominal range are not rejected: the transform is affine and the result saturates,
/// which is the correct handling of the sub-black and super-white codes a limited-range plane
/// legally carries.
///
/// The derivation is exact integer arithmetic and the conversion is fixed-point, so this path is
/// bit-exact and deterministic — unlike the crate's `f64` colour science.
///
/// See the [module documentation](self) for why this is *not* built on [`ycbcr_to_rgb`].
///
/// # Examples
///
/// ```
/// use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, YcbcrMatrix};
/// // 10-bit BT.2020 non-constant-luminance, studio swing — the HDR HEIC/AVIF case.
/// let m = YcbcrMatrix::new(MatrixCoefficients::Bt2020Ncl, ColorRange::Limited, BitDepth::Ten)?;
/// assert_eq!(m.to_rgb(64, 512, 512), (0, 0, 0)); // video black
/// assert_eq!(m.to_rgb(940, 512, 512), (1023, 1023, 1023)); // video white
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct YcbcrMatrix {
    /// Luma gain, `max / luma_scale` in the [`MATRIX_FIX`] scale.
    y_gain: i32,
    /// The `R = Y + c·Cr` coefficient, pre-scaled by `max / chroma_scale`.
    r_cr: i32,
    /// The `G = Y + c·Cb` coefficient (negative), pre-scaled the same way.
    g_cb: i32,
    /// The `G = Y + c·Cr` coefficient (negative), pre-scaled the same way.
    g_cr: i32,
    /// The `B = Y + c·Cb` coefficient, pre-scaled the same way.
    b_cb: i32,
    /// Luma offset subtracted before scaling: `16 << (bits - 8)` limited, `0` full.
    y_offset: i32,
    /// Chroma offset subtracted before scaling: `128 << (bits - 8)` limited, `1 << (bits - 1)` full.
    c_offset: i32,
    /// The depth of both the input planes and the output samples.
    depth: BitDepth,
}

/// The `(Kr, Kb)` luma weights of `matrix`, as numerators over [`K_DEN`] (ITU-T H.273 Table 4), or
/// `None` for coefficients that are not a `Kr`/`Kb` de-matrixing at all.
fn luma_weights(matrix: MatrixCoefficients) -> Option<(i128, i128)> {
    match matrix {
        MatrixCoefficients::Bt709 => Some((2126, 722)),
        // BT.470 System B,G (code point 5) and BT.601 (6) name the same de-matrixing.
        MatrixCoefficients::Bt470Bg | MatrixCoefficients::Bt601 => Some((2990, 1140)),
        MatrixCoefficients::Bt2020Ncl => Some((2627, 593)),
        _ => None,
    }
}

/// The `(luma offset, luma scale, chroma offset, chroma scale, max)` of `range` at `bits`
/// (ITU-T H.273 §8.3): studio swing scales the 16/219/128/224 anchors by `1 << (bits - 8)`, while
/// full range spans the whole code space with chroma centred at the midpoint.
fn range_params(range: ColorRange, bits: u32) -> (i128, i128, i128, i128, i128) {
    let max = (1i128 << bits) - 1;
    match range {
        ColorRange::Limited => {
            let s = 1i128 << (bits - 8);
            (16 * s, 219 * s, 128 * s, 224 * s, max)
        }
        ColorRange::Full => (0, max, 1i128 << (bits - 1), max, max),
    }
}

impl YcbcrMatrix {
    /// Derives the de-matrixing for `matrix` at `range` and `bit_depth`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for coefficients that are not a non-constant-luminance
    /// `Kr`/`Kb` de-matrixing:
    ///
    /// - [`MatrixCoefficients::Identity`] — a GBR plane permutation requiring 4:4:4, not an affine
    ///   transform; the caller reorders the planes itself.
    /// - [`MatrixCoefficients::YCgCo`] — a different (lifting-based) transform family.
    /// - [`MatrixCoefficients::Unspecified`] — choosing a default is a *policy* decision belonging
    ///   to the format layer, which knows its container's conventions; this primitive will not
    ///   silently pick one.
    /// - Any code point a later minor release adds without support here.
    pub fn new(matrix: MatrixCoefficients, range: ColorRange, bit_depth: BitDepth) -> Result<Self> {
        let (kr, kb) = luma_weights(matrix).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "YCbCr matrix coefficients are not a Kr/Kb de-matrixing (identity, YCgCo, and \
                 unspecified are the caller's to resolve)",
            )
        })?;
        let kg = K_DEN - kr - kb;
        let bits = u32::from(bit_depth.bits());
        let (y_off, y_scale, c_off, c_scale, max) = range_params(range, bits);
        let scale = 1i128 << MATRIX_FIX;

        // R = Y + 2(1 - Kr)·Cr and B = Y + 2(1 - Kb)·Cb; the G coefficients are
        // -2·Kb(1 - Kb)/Kg and -2·Kr(1 - Kr)/Kg. Each is pre-multiplied by max/chroma_scale so the
        // stored value maps a raw chroma sample straight onto an output sample.
        let chroma_den = c_scale * K_DEN;
        let y_gain = round_div(max * scale, y_scale);
        let r_cr = round_div(max * scale * 2 * (K_DEN - kr), chroma_den);
        let b_cb = round_div(max * scale * 2 * (K_DEN - kb), chroma_den);
        let g_cb = -round_div(max * scale * 2 * kb * (K_DEN - kb), chroma_den * kg);
        let g_cr = -round_div(max * scale * 2 * kr * (K_DEN - kr), chroma_den * kg);

        Ok(Self {
            y_gain: y_gain as i32,
            r_cr: r_cr as i32,
            g_cb: g_cb as i32,
            g_cr: g_cr as i32,
            b_cb: b_cb as i32,
            y_offset: y_off as i32,
            c_offset: c_off as i32,
            depth: bit_depth,
        })
    }

    /// The de-matrixing for a monochrome (4:0:0) plane: luma range expansion only, with both
    /// chroma coefficients zero.
    ///
    /// Infallible — luma scaling does not depend on the matrix coefficients, and a monochrome plane
    /// carries none. [`to_rgb`](Self::to_rgb) on this matrix returns a neutral gray triple whatever
    /// chroma it is handed, equal to [`expand_gray`](Self::expand_gray) in all three channels.
    #[must_use]
    pub fn monochrome(range: ColorRange, bit_depth: BitDepth) -> Self {
        let bits = u32::from(bit_depth.bits());
        let (y_off, y_scale, c_off, _, max) = range_params(range, bits);
        Self {
            y_gain: round_div(max * (1i128 << MATRIX_FIX), y_scale) as i32,
            r_cr: 0,
            g_cb: 0,
            g_cr: 0,
            b_cb: 0,
            y_offset: y_off as i32,
            c_offset: c_off as i32,
            depth: bit_depth,
        }
    }

    /// Converts one YCbCr triple to RGB, each component in `0..=(1 << bit_depth) - 1`.
    #[must_use]
    #[inline]
    pub fn to_rgb(self, y: u16, cb: u16, cr: u16) -> (u16, u16, u16) {
        let bits = u32::from(self.depth.bits());
        let yy = i64::from(self.y_gain) * (i64::from(y) - i64::from(self.y_offset));
        let u = i64::from(cb) - i64::from(self.c_offset);
        let v = i64::from(cr) - i64::from(self.c_offset);
        (
            round_clip(yy + i64::from(self.r_cr) * v, bits),
            round_clip(
                yy + i64::from(self.g_cb) * u + i64::from(self.g_cr) * v,
                bits,
            ),
            round_clip(yy + i64::from(self.b_cb) * u, bits),
        )
    }

    /// Converts one YCbCr triple whose chroma is given in **sixteenths** of a code value to RGB,
    /// each component in `0..=(1 << bit_depth) - 1`.
    ///
    /// `cb` and `cr` are 16 times the chroma value, which is the unrounded weighted sum
    /// [`crate::upsample::libavif::bilinear_row`] returns. The conversion is
    /// [`to_rgb`](Self::to_rgb)'s with the chroma's four fractional bits carried through to the
    /// single final rounding, so `to_rgb_sixteenths(y, 16 * cb, 16 * cr)` equals
    /// `to_rgb(y, cb, cr)`. Rounding interpolated chroma to whole code values first would add up
    /// to half a chroma step of error before the conversion. libavif, the AVIF reference, never
    /// takes that step.
    #[must_use]
    #[inline]
    pub fn to_rgb_sixteenths(self, y: u16, cb: u32, cr: u32) -> (u16, u16, u16) {
        let bits = u32::from(self.depth.bits());
        let frac = MATRIX_FIX + SIXTEENTHS_FIX;
        let yy =
            (i64::from(self.y_gain) * (i64::from(y) - i64::from(self.y_offset))) << SIXTEENTHS_FIX;
        let offset = i64::from(self.c_offset) << SIXTEENTHS_FIX;
        let u = i64::from(cb) - offset;
        let v = i64::from(cr) - offset;
        (
            round_clip_at(yy + i64::from(self.r_cr) * v, frac, bits),
            round_clip_at(
                yy + i64::from(self.g_cb) * u + i64::from(self.g_cr) * v,
                frac,
                bits,
            ),
            round_clip_at(yy + i64::from(self.b_cb) * u, frac, bits),
        )
    }

    /// Expands one monochrome luma sample to a display gray level — the luma-only path, exactly
    /// [`to_rgb`](Self::to_rgb)'s first channel at neutral chroma.
    ///
    /// Identity (saturated) for [`ColorRange::Full`]; the studio-swing expansion
    /// `(y - 16·2^(bits-8)) · max / (219·2^(bits-8))` for [`ColorRange::Limited`].
    #[must_use]
    #[inline]
    pub fn expand_gray(self, y: u16) -> u16 {
        let bits = u32::from(self.depth.bits());
        round_clip(
            i64::from(self.y_gain) * (i64::from(y) - i64::from(self.y_offset)),
            bits,
        )
    }

    /// The bit depth of the input planes and the RGB samples this matrix produces.
    #[must_use]
    pub fn bit_depth(self) -> BitDepth {
        self.depth
    }
}

/// A precomputed non-constant-luminance RGB → YCbCr matrixing (ITU-T H.273 §8.3) for one
/// (matrix coefficients, range, bit depth) triple — the **coding** direction, and the exact
/// inverse-in-intent of [`YcbcrMatrix`].
///
/// An encoder builds one per image and calls [`from_rgb`](Self::from_rgb) per pixel; the two types
/// share this module's weight table, range anchors and rounding, so the pair cannot drift apart.
/// The split is by *direction* rather than one type with two halves because the two are not needed
/// together: a decoder never matrixes, and a monochrome plane
/// ([`YcbcrMatrix::monochrome`]) has a de-matrixing but no luma weights to matrix *with*.
///
/// Samples are `u16` at every depth with a `bit_depth`-bit value in the low bits, matching
/// [`YcbcrMatrix`]. Inputs above the depth's maximum are not rejected; results saturate.
///
/// The derivation is exact integer arithmetic and the conversion is fixed-point, so this path is
/// bit-exact and deterministic.
///
/// # Examples
///
/// ```
/// use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, RgbToYcbcr};
///
/// let m = RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight)?;
/// // Neutral grey stays neutral: chroma centred, luma unchanged.
/// assert_eq!(m.from_rgb(128, 128, 128), (128, 128, 128));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbToYcbcr {
    /// Luma row over `(R, G, B)`, in the [`MATRIX_FIX`] scale.
    y_row: [i32; 3],
    /// Cb row over `(R, G, B)`, in the [`MATRIX_FIX`] scale.
    cb_row: [i32; 3],
    /// Cr row over `(R, G, B)`, in the [`MATRIX_FIX`] scale.
    cr_row: [i32; 3],
    /// Luma pedestal added after matrixing: `16 << (bits - 8)` limited, `0` full.
    y_offset: i32,
    /// Chroma centre added after matrixing: `128 << (bits - 8)` limited, `1 << (bits - 1)` full.
    c_offset: i32,
    /// The depth of both the RGB inputs and the coded planes.
    depth: BitDepth,
}

impl RgbToYcbcr {
    /// Derives the matrixing for `matrix` at `range` and `bit_depth`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for exactly the coefficients [`YcbcrMatrix::new`] rejects,
    /// and for the same reasons: [`MatrixCoefficients::Identity`] (the encoder writes GBR planes
    /// itself — see [`Planar8::from_rgb8_identity`](crate::Planar8::from_rgb8_identity)),
    /// [`MatrixCoefficients::YCgCo`], [`MatrixCoefficients::Unspecified`], and any code point a
    /// later minor release adds without support here.
    pub fn new(matrix: MatrixCoefficients, range: ColorRange, bit_depth: BitDepth) -> Result<Self> {
        let (kr, kb) = luma_weights(matrix).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "YCbCr matrix coefficients are not a Kr/Kb matrixing (identity, YCgCo, and \
                 unspecified are the caller's to resolve)",
            )
        })?;
        let kg = K_DEN - kr - kb;
        let bits = u32::from(bit_depth.bits());
        let (y_off, y_scale, c_off, c_scale, max) = range_params(range, bits);
        let scale = 1i128 << MATRIX_FIX;

        // Y' = (Kr·R + Kg·G + Kb·B) scaled from the 0..=max input span onto the luma excursion;
        // Cb = (B - Y')/(2(1 - Kb)) and Cr = (R - Y')/(2(1 - Kr)) onto the chroma excursion. Each
        // row is pre-divided by `max` so a raw RGB sample maps straight onto a coded sample.
        let luma_den = K_DEN * max;
        let row = |num: i128, den: i128| round_div(num * scale, den);
        let (cb_den, cr_den) = (2 * (K_DEN - kb) * max, 2 * (K_DEN - kr) * max);

        Ok(Self {
            y_row: [
                row(y_scale * kr, luma_den) as i32,
                row(y_scale * kg, luma_den) as i32,
                row(y_scale * kb, luma_den) as i32,
            ],
            cb_row: [
                -row(c_scale * kr, cb_den) as i32,
                -row(c_scale * kg, cb_den) as i32,
                row(c_scale * (K_DEN - kb), cb_den) as i32,
            ],
            cr_row: [
                row(c_scale * (K_DEN - kr), cr_den) as i32,
                -row(c_scale * kg, cr_den) as i32,
                -row(c_scale * kb, cr_den) as i32,
            ],
            y_offset: y_off as i32,
            c_offset: c_off as i32,
            depth: bit_depth,
        })
    }

    /// Converts one R'G'B' triple to Y'CbCr, each component in `0..=(1 << bit_depth) - 1`.
    #[must_use]
    #[inline]
    pub fn from_rgb(self, r: u16, g: u16, b: u16) -> (u16, u16, u16) {
        let bits = u32::from(self.depth.bits());
        let (r, g, b) = (i64::from(r), i64::from(g), i64::from(b));
        let dot =
            |row: [i32; 3]| i64::from(row[0]) * r + i64::from(row[1]) * g + i64::from(row[2]) * b;
        let bias = |offset: i32| i64::from(offset) << MATRIX_FIX;
        (
            round_clip(dot(self.y_row) + bias(self.y_offset), bits),
            round_clip(dot(self.cb_row) + bias(self.c_offset), bits),
            round_clip(dot(self.cr_row) + bias(self.c_offset), bits),
        )
    }

    /// The bit depth of both the RGB inputs and the YCbCr samples this matrix produces.
    #[must_use]
    pub fn bit_depth(self) -> BitDepth {
        self.depth
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::luminance::LUMA_FIX;

    use super::*;
    use crate::cicp::ColorRange::{Full, Limited};

    #[test]
    fn full_range_luma_row_comes_from_gamut_core_unchanged() {
        // The luma row is no longer written down here, so this pins the two things that make the
        // substitution safe: the shared fixed-point scale, and the exact coefficients that were
        // inlined before. A drift in either would silently reshade every full-range conversion.
        // FIX stays independent of LUMA_FIX because it also scales the chroma rows below, which
        // have no counterpart in gamut-core -- so the two are asserted equal rather than aliased.
        assert_eq!(FIX, LUMA_FIX as i32);
        assert_eq!([LUMA_R, LUMA_G, LUMA_B], [19_595, 38_470, 7_471]);

        // And the observable result: every value the old literals produced, still produced. The
        // sweep covers the whole cube coarsely plus the endpoints where rounding is decided.
        for r in (0..=255u8).step_by(17) {
            for g in (0..=255u8).step_by(17) {
                for b in (0..=255u8).step_by(17) {
                    let expected = (i32::from(r) * 19_595
                        + i32::from(g) * 38_470
                        + i32::from(b) * 7_471
                        + HALF)
                        >> FIX;
                    let (y, _, _) = rgb_to_ycbcr(r, g, b, Full);
                    assert_eq!(i32::from(y), expected, "luma drifted at ({r}, {g}, {b})");
                }
            }
        }
    }

    #[test]
    fn full_range_color_anchors() {
        // Standard JFIF/BT.601 full-range values: black/white span the whole luma range.
        assert_eq!(rgb_to_ycbcr(0, 0, 0, Full), (0, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 255, 255, Full), (255, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 0, 0, Full), (76, 85, 255));
        let (y, cb, cr) = rgb_to_ycbcr(128, 128, 128, Full);
        assert_eq!((cb, cr), (128, 128));
        assert!((i32::from(y) - 128).abs() <= 1);
        // Neutral gray inverts to itself. Every channel lands on 128 — away from the 0/255 clamp —
        // so the `+ HALF` rounding term of each `ycbcr_to_rgb` component is observable (a mutated
        // `- HALF` shifts each result to 127).
        assert_eq!(ycbcr_to_rgb(128, 128, 128, Full), (128, 128, 128));
    }

    #[test]
    fn limited_range_matches_libwebp_anchors() {
        // Studio swing: black→16, white→235, neutral chroma 128. Red matches libwebp's per-pixel
        // VP8RGBToY/U/V (src/dsp/yuv.h) exactly, pinning the port independent of the FFI oracle.
        assert_eq!(rgb_to_ycbcr(0, 0, 0, Limited), (16, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 255, 255, Limited), (235, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 0, 0, Limited), (82, 90, 240));
        // The inverse round-trips that red back to (near) pure red via libwebp's VP8YUVToR/G/B.
        let (r, g, b) = ycbcr_to_rgb(82, 90, 240, Limited);
        assert!(
            r >= 254 && g <= 2 && b <= 2,
            "limited red inverse = ({r},{g},{b})"
        );
    }

    #[test]
    fn pixel_roundtrip_within_tolerance() {
        // The forward/inverse pair recovers RGB within a few units (no subsampling), in both ranges.
        let colors = [
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (10, 200, 90),
            (123, 45, 67),
            (200, 200, 50),
            (17, 17, 200),
        ];
        for range in [Full, Limited] {
            for (r, g, b) in colors {
                let (y, cb, cr) = rgb_to_ycbcr(r, g, b, range);
                let (r2, g2, b2) = ycbcr_to_rgb(y, cb, cr, range);
                let err = (i32::from(r) - i32::from(r2)).abs().max(
                    (i32::from(g) - i32::from(g2))
                        .abs()
                        .max((i32::from(b) - i32::from(b2)).abs()),
                );
                assert!(
                    err <= 4,
                    "{range:?} color ({r},{g},{b}) round-trip error {err}"
                );
            }
        }
    }

    #[test]
    fn new_validates_plane_lengths() {
        assert!(Yuv420::new(4, 4, vec![0; 16], vec![0; 4], vec![0; 4]).is_ok());
        assert!(Yuv420::new(4, 4, vec![0; 16], vec![0; 3], vec![0; 4]).is_err());
        assert!(Yuv420::new(4, 4, vec![0; 15], vec![0; 4], vec![0; 4]).is_err());
    }

    #[test]
    fn rejects_bad_rgb_length() {
        assert!(Yuv420::from_rgb8(&[0, 1, 2, 3], 1, 1, Limited).is_err());
        assert!(Yuv420::from_rgb8(&[], 0, 1, Limited).is_err());
    }

    #[test]
    fn rejects_overflowing_dimensions() {
        // Near-max dimensions must yield Err, not an overflow panic (debug) or a wrapped length
        // check (32-bit release): width * height * 3 exceeds usize even on 64-bit targets.
        assert!(Yuv420::from_rgb8(&[], u32::MAX, u32::MAX, Limited).is_err());
        assert!(Yuv420::new(u32::MAX, u32::MAX, vec![], vec![], vec![]).is_err());
    }

    #[test]
    fn vp8_clip8_fast_path_and_clamps() {
        // In-range values (`v & !MASK2 == 0`) take the `>> FIX2` fast path; out-of-range values clamp
        // hard to 0 (low) or 255 (high). The negative case pins `v < 0` against `v == 0`.
        assert_eq!(vp8_clip8(0), 0);
        assert_eq!(vp8_clip8(640), 10); // 640 >> 6
        assert_eq!(vp8_clip8(MASK2), (MASK2 >> FIX2) as u8); // largest in-range value (= 255)
        assert_eq!(vp8_clip8(MASK2 + 1), 255); // first out-of-range high
        assert_eq!(vp8_clip8(-1), 0); // out-of-range low
    }

    #[test]
    fn box_subsample_matches_reference_for_varying_image() {
        // A spatially-varying 5x3 image (odd dims ⇒ partial edge blocks, incl. a 1-pixel corner) so
        // the chroma box-average exercises the source coordinates `cx*2`/`cy*2` and the per-block
        // `count` rounding. An independent reference re-derives the expected U/V; any mutated
        // coordinate or rounding in `from_rgb8` diverges from it.
        let (w, h) = (5usize, 3usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = (x * 50).min(255) as u8;
                rgb[i + 1] = (y * 90).min(255) as u8;
                rgb[i + 2] = ((x + y) * 35).min(255) as u8;
            }
        }
        let yuv = Yuv420::from_rgb8(&rgb, w as u32, h as u32, Full).unwrap();
        // 5x3 luma ⇒ chroma is ceil(5/2) x ceil(3/2) = 3 x 2, as literals — re-deriving them from
        // chroma_width/chroma_height here would let a broken round-up hide. The odd sizes pin it.
        assert_eq!(Yuv420::chroma_width(w as u32), 3);
        assert_eq!(Yuv420::chroma_height(h as u32), 2);
        assert_eq!(yuv.y().len(), 15);
        assert_eq!((yuv.u().len(), yuv.v().len()), (6, 6));
        let (cw, ch) = (3usize, 2usize);
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut su, mut sv, mut count) = (0u32, 0u32, 0u32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                        if px < w && py < h {
                            let i = (py * w + px) * 3;
                            let (_, cb, cr) = rgb_to_ycbcr(rgb[i], rgb[i + 1], rgb[i + 2], Full);
                            su += u32::from(cb);
                            sv += u32::from(cr);
                            count += 1;
                        }
                    }
                }
                assert_eq!(
                    yuv.u()[cy * cw + cx],
                    ((su + count / 2) / count) as u8,
                    "u[{cx},{cy}]"
                );
                assert_eq!(
                    yuv.v()[cy * cw + cx],
                    ((sv + count / 2) / count) as u8,
                    "v[{cx},{cy}]"
                );
            }
        }
    }

    #[test]
    fn to_rgb8_nearest_replicates_chroma() {
        // Distinct per-position Y/U/V so the nearest-chroma index `(py/2)*cw + (px/2)` and the luma
        // index `py*w + px` both matter; an independent reference upsamples identically. An empty
        // `vec![]` body, a mutated index, or wrong index arithmetic all diverge.
        let (w, h) = (5u32, 3u32);
        let cw = Yuv420::chroma_width(w);
        let chh = Yuv420::chroma_height(h);
        let y: Vec<u8> = (0..w * h).map(|i| (i * 7 % 251) as u8).collect();
        let u: Vec<u8> = (0..cw * chh).map(|i| (30 + i * 17 % 200) as u8).collect();
        let v: Vec<u8> = (0..cw * chh).map(|i| (220 - i * 13 % 200) as u8).collect();
        let yuv = Yuv420::new(w, h, y.clone(), u.clone(), v.clone()).unwrap();
        assert_eq!((yuv.width(), yuv.height()), (w, h));
        let out = yuv.to_rgb8(Full, ChromaUpsampling::Nearest);
        assert_eq!(out.len(), (w * h * 3) as usize);
        let (wu, cwu) = (w as usize, cw as usize);
        for py in 0..h as usize {
            for px in 0..wu {
                let ci = (py / 2) * cwu + (px / 2);
                let (r, g, b) = ycbcr_to_rgb(y[py * wu + px], u[ci], v[ci], Full);
                let o = (py * wu + px) * 3;
                assert_eq!(
                    (out[o], out[o + 1], out[o + 2]),
                    (r, g, b),
                    "pixel ({px},{py})"
                );
            }
        }
    }

    #[test]
    fn to_rgb8_of_an_empty_buffer_is_empty() {
        // `new` admits zero dimensions; converting them must not panic in either mode.
        for (w, h) in [(0u32, 3u32), (3, 0), (0, 0)] {
            let chroma = (Yuv420::chroma_width(w) * Yuv420::chroma_height(h)) as usize;
            let yuv = Yuv420::new(
                w,
                h,
                vec![0; (w * h) as usize],
                vec![0; chroma],
                vec![0; chroma],
            )
            .unwrap();
            for upsampling in [ChromaUpsampling::Nearest, ChromaUpsampling::Interpolated] {
                assert!(
                    yuv.to_rgb8(Limited, upsampling).is_empty(),
                    "{w}x{h} {upsampling:?}"
                );
            }
        }
    }

    #[test]
    fn to_rgb8_interpolated_runs_libwebps_fancy_upsampler_per_plane() {
        // Each output row interpolates between chroma row `py / 2` and its neighbour above (even
        // rows) or below (odd rows), clamped to the plane: libwebp's `EmitFancyRGB` row order. U
        // and V carry different data, so swapping them, reading the wrong row, or replicating
        // instead of interpolating all diverge from this independently assembled expectation.
        let (w, h) = (5u32, 5u32);
        let (cw, ch) = (3usize, 3usize);
        let y: Vec<u8> = (0..w * h).map(|i| (16 + i * 9 % 219) as u8).collect();
        let u: Vec<u8> = (0..9u32).map(|i| (40 + i * 23 % 180) as u8).collect();
        let v: Vec<u8> = (0..9u32).map(|i| (210 - i * 19 % 170) as u8).collect();
        let yuv = Yuv420::new(w, h, y.clone(), u.clone(), v.clone()).unwrap();
        let out = yuv.to_rgb8(Limited, ChromaUpsampling::Interpolated);
        let rows: [(usize, usize); 5] = [(0, 0), (0, 1), (1, 0), (1, 2), (2, 1)];
        let wu = w as usize;
        for (py, &(near, far)) in rows.iter().enumerate() {
            let row = |p: &[u8], r: usize| p[r * cw..(r + 1) * cw].to_vec();
            let (mut ur, mut vr) = (vec![0u8; wu], vec![0u8; wu]);
            libwebp::fancy_row(&row(&u, near), &row(&u, far), &mut ur);
            libwebp::fancy_row(&row(&v, near), &row(&v, far), &mut vr);
            for px in 0..wu {
                let (r, g, b) = ycbcr_to_rgb(y[py * wu + px], ur[px], vr[px], Limited);
                let o = (py * wu + px) * 3;
                assert_eq!(&out[o..o + 3], &[r, g, b], "pixel ({px},{py})");
            }
        }
        assert_eq!(ch, Yuv420::chroma_height(h) as usize);
        // Interpolation really happened: it differs from replication on this content.
        assert_ne!(out, yuv.to_rgb8(Limited, ChromaUpsampling::Nearest));
    }

    // ---- YcbcrMatrix ------------------------------------------------------------------------

    use gamut_core::ErrorKind;

    use crate::cicp::MatrixCoefficients::{
        Bt470Bg, Bt601, Bt709, Bt2020Ncl, Identity, Unspecified, YCgCo,
    };
    use crate::format::BitDepth::{Eight, Sixteen, Ten, Twelve};

    /// Every modeled `(matrix, range, depth)` triple, for the sweeps below.
    const CONFIGS: [(MatrixCoefficients, ColorRange, BitDepth); 24] = [
        (Bt709, Limited, Eight),
        (Bt709, Limited, Ten),
        (Bt709, Limited, Twelve),
        (Bt709, Limited, Sixteen),
        (Bt709, Full, Eight),
        (Bt709, Full, Ten),
        (Bt709, Full, Twelve),
        (Bt709, Full, Sixteen),
        (Bt601, Limited, Eight),
        (Bt601, Limited, Ten),
        (Bt601, Limited, Twelve),
        (Bt601, Limited, Sixteen),
        (Bt601, Full, Eight),
        (Bt601, Full, Ten),
        (Bt601, Full, Twelve),
        (Bt601, Full, Sixteen),
        (Bt2020Ncl, Limited, Eight),
        (Bt2020Ncl, Limited, Ten),
        (Bt2020Ncl, Limited, Twelve),
        (Bt2020Ncl, Limited, Sixteen),
        (Bt2020Ncl, Full, Eight),
        (Bt2020Ncl, Full, Ten),
        (Bt2020Ncl, Full, Twelve),
        (Bt2020Ncl, Full, Sixteen),
    ];

    /// The exactly-derived Q20 coefficients `[y_gain, r_cr, g_cb, g_cr, b_cb]`, computed offline
    /// from the H.273 §8.3 equations with exact rational arithmetic. This literal table is the
    /// audit trail for the shipped derivation: it is asserted **equal**, not approximate.
    const COEFFICIENTS: [[i32; 5]; 24] = [
        [1220945, 1879825, -223607, -558796, 2215014],
        [1224536, 1885354, -224265, -560439, 2221529],
        [1225433, 1886736, -224429, -560850, 2223157],
        [1225714, 1887168, -224481, -560979, 2223666],
        [1048576, 1651297, -196424, -490864, 1945738],
        [1048576, 1651297, -196424, -490864, 1945738],
        [1048576, 1651297, -196424, -490864, 1945738],
        [1048576, 1651297, -196424, -490864, 1945738],
        [1220945, 1673555, -410793, -852458, 2115221],
        [1224536, 1678478, -412001, -854966, 2121442],
        [1225433, 1679708, -412303, -855592, 2122998],
        [1225714, 1680093, -412397, -855788, 2123484],
        [1048576, 1470104, -360853, -748826, 1858077],
        [1048576, 1470104, -360853, -748826, 1858077],
        [1048576, 1470104, -360853, -748826, 1858077],
        [1048576, 1470104, -360853, -748826, 1858077],
        [1220945, 1760217, -196426, -682019, 2245811],
        [1224536, 1765394, -197003, -684025, 2252416],
        [1225433, 1766689, -197148, -684527, 2254068],
        [1225714, 1767093, -197193, -684683, 2254584],
        [1048576, 1546230, -172546, -599107, 1972791],
        [1048576, 1546230, -172546, -599107, 1972791],
        [1048576, 1546230, -172546, -599107, 1972791],
        [1048576, 1546230, -172546, -599107, 1972791],
    ];

    /// `(black, white)` luma and the neutral chroma code for `(range, depth)`.
    fn anchors(range: ColorRange, depth: BitDepth) -> (u16, u16, u16) {
        let bits = u32::from(depth.bits());
        match range {
            Limited => {
                let s = 1u16 << (bits - 8);
                (16 * s, 235 * s, 128 * s)
            }
            Full => (0, depth.max_value(), 1u16 << (bits - 1)),
        }
    }

    #[test]
    fn derived_coefficients_match_literals() {
        for (i, &(matrix, range, depth)) in CONFIGS.iter().enumerate() {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let got = [m.y_gain, m.r_cr, m.g_cb, m.g_cr, m.b_cb];
            assert_eq!(got, COEFFICIENTS[i], "{matrix:?} {range:?} {depth:?}");
        }
    }

    #[test]
    fn derived_offsets_match_the_range_anchors() {
        for &(matrix, range, depth) in &CONFIGS {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let (black, _, neutral) = anchors(range, depth);
            assert_eq!(
                (m.y_offset, m.c_offset),
                (i32::from(black), i32::from(neutral)),
                "{matrix:?} {range:?} {depth:?}"
            );
        }
    }

    #[test]
    fn bt2020_ten_bit_limited_anchors_are_golden() {
        let m = YcbcrMatrix::new(Bt2020Ncl, Limited, Ten).unwrap();
        // Range endpoints.
        assert_eq!(m.to_rgb(64, 512, 512), (0, 0, 0));
        assert_eq!(m.to_rgb(940, 512, 512), (1023, 1023, 1023));
        // The three BT.2020 primaries: the H.273 forward equations evaluated at full-amplitude R,
        // G and B round-trip back **exactly**, which pins each coefficient independently.
        assert_eq!(m.to_rgb(294, 387, 960), (1023, 0, 0));
        assert_eq!(m.to_rgb(658, 189, 100), (0, 1023, 0));
        assert_eq!(m.to_rgb(116, 960, 476), (0, 0, 1023));
        // Mid code value, far from both clamps.
        assert_eq!(m.to_rgb(512, 512, 512), (523, 523, 523));
    }

    #[test]
    fn matrices_disagree_on_identical_planes() {
        // A tolerance-based test passes even if the matrix argument is ignored; this does not.
        let (y, cb, cr) = (600, 300, 700);
        let of = |mc| {
            YcbcrMatrix::new(mc, Limited, Ten)
                .unwrap()
                .to_rgb(y, cb, cr)
        };
        let (a, b, c) = (of(Bt709), of(Bt601), of(Bt2020Ncl));
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // Range is likewise load-bearing.
        let full = YcbcrMatrix::new(Bt2020Ncl, Full, Ten)
            .unwrap()
            .to_rgb(y, cb, cr);
        assert_ne!(c, full);
    }

    #[test]
    fn rounding_addend_is_observable() {
        // Anchors whose exact values straddle a half, so dropping or negating the rounding addend
        // changes the result. A `±1` tolerance test cannot catch that class of mutation.
        let m = YcbcrMatrix::new(Bt2020Ncl, Limited, Ten).unwrap();
        // Exact R = 523.1782 — rounds up only with the addend present.
        assert_eq!(m.to_rgb(512, 512, 512).0, 523);
        // Exact R = 511.5001 — the tie-breaking case.
        assert_eq!(m.to_rgb(502, 512, 512).0, 512);
        // Exact R = 4.6575 at 8-bit BT.601.
        let m8 = YcbcrMatrix::new(Bt601, Limited, Eight).unwrap();
        assert_eq!(m8.to_rgb(20, 128, 128).0, 5);
    }

    #[test]
    fn black_and_white_are_exact_at_every_depth_and_range() {
        // Testing more than one depth per (matrix, range) is load-bearing: a hard-coded `8`, or a
        // shift in the wrong direction, cancels at 8-bit and only shows up at 10/12/16. The 16-bit
        // rows are also the `i64` accumulator regression — an `i32` accumulator overflows here.
        for &(matrix, range, depth) in &CONFIGS {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let (black, white, neutral) = anchors(range, depth);
            let max = depth.max_value();
            assert_eq!(
                m.to_rgb(black, neutral, neutral),
                (0, 0, 0),
                "black {matrix:?} {range:?} {depth:?}"
            );
            assert_eq!(
                m.to_rgb(white, neutral, neutral),
                (max, max, max),
                "white {matrix:?} {range:?} {depth:?}"
            );
        }
    }

    #[test]
    fn out_of_range_samples_clamp_rather_than_wrap() {
        let m = YcbcrMatrix::new(Bt2020Ncl, Limited, Ten).unwrap();
        // Sub-black and super-white luma: raw values are negative / past the maximum.
        assert_eq!(m.to_rgb(0, 512, 512), (0, 0, 0));
        assert_eq!(m.to_rgb(1023, 512, 512), (1023, 1023, 1023));
        // Chroma extremes. Both G coefficients are negative, so G moves *opposite* to R and B and
        // lands mid-range — a dropped term or a raw cast produces a wildly different number here
        // rather than another saturated one.
        assert_eq!(m.to_rgb(64, 0, 0), (0, 430, 0));
        assert_eq!(m.to_rgb(940, 1023, 1023), (1023, 594, 1023));
    }

    /// An independent `f64` reference for H.273 §8.3, written from the published equations rather
    /// than from the implementation: its own `(Kr, Kb)`, its own normalization, its own rounding.
    fn reference(
        matrix: MatrixCoefficients,
        range: ColorRange,
        depth: BitDepth,
        y: u16,
        cb: u16,
        cr: u16,
    ) -> (u16, u16, u16) {
        let (kr, kb) = match matrix {
            Bt709 => (0.2126_f64, 0.0722_f64),
            Bt601 | Bt470Bg => (0.299, 0.114),
            Bt2020Ncl => (0.2627, 0.0593),
            _ => unreachable!("reference covers the supported matrices only"),
        };
        let bits = u32::from(depth.bits());
        let max = f64::from(depth.max_value());
        let (yn, cbn, crn) = match range {
            Limited => {
                let s = f64::from(1u32 << (bits - 8));
                (
                    (f64::from(y) - 16.0 * s) / (219.0 * s),
                    (f64::from(cb) - 128.0 * s) / (224.0 * s),
                    (f64::from(cr) - 128.0 * s) / (224.0 * s),
                )
            }
            Full => {
                let mid = f64::from(1u32 << (bits - 1));
                (
                    f64::from(y) / max,
                    (f64::from(cb) - mid) / max,
                    (f64::from(cr) - mid) / max,
                )
            }
        };
        let kg = 1.0 - kr - kb;
        let r = yn + 2.0 * (1.0 - kr) * crn;
        let b = yn + 2.0 * (1.0 - kb) * cbn;
        let g = yn - (2.0 * kb * (1.0 - kb) / kg) * cbn - (2.0 * kr * (1.0 - kr) / kg) * crn;
        let q = |v: f64| (v.clamp(0.0, 1.0) * max).round() as u16;
        (q(r), q(g), q(b))
    }

    #[test]
    fn matches_an_independent_f64_reference() {
        // Deterministic LCG (Numerical Recipes constants) — no `rand` dependency, and the same
        // sweep every run so a failure is reproducible.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 32) as u16
        };
        for &(matrix, range, depth) in &CONFIGS {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let max = depth.max_value();
            // `max + 1` would overflow `u16` at 16-bit, so reduce in `u32`.
            let span = u32::from(max) + 1;
            let mut sample = || (u32::from(next()) % span) as u16;
            for _ in 0..2048 {
                let (y, cb, cr) = (sample(), sample(), sample());
                let got = m.to_rgb(y, cb, cr);
                let want = reference(matrix, range, depth, y, cb, cr);
                // The tolerance is the Q20 quantization bound; the great majority of samples land
                // on the exactly-rounded value. `rounding_addend_is_observable` exists precisely
                // because this tolerance cannot kill a rounding mutation.
                for (a, b) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)] {
                    assert!(
                        a.abs_diff(b) <= 1,
                        "{matrix:?} {range:?} {depth:?} ({y},{cb},{cr}): got {got:?} want {want:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_matrices_are_rejected() {
        for mc in [Identity, Unspecified, MatrixCoefficients::YCgCo] {
            let error = YcbcrMatrix::new(mc, Limited, Ten).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{mc:?}");
        }
        for mc in [Bt709, Bt601, Bt470Bg, Bt2020Ncl] {
            for range in [Limited, Full] {
                for depth in [Eight, Ten, Twelve, Sixteen] {
                    assert!(YcbcrMatrix::new(mc, range, depth).is_ok(), "{mc:?}");
                }
            }
        }
    }

    #[test]
    fn bt470bg_is_bt601() {
        for range in [Limited, Full] {
            for depth in [Eight, Ten, Twelve, Sixteen] {
                assert_eq!(
                    YcbcrMatrix::new(Bt470Bg, range, depth).unwrap(),
                    YcbcrMatrix::new(Bt601, range, depth).unwrap(),
                );
            }
        }
        assert_eq!(MatrixCoefficients::from_code_point(5), Some(Bt470Bg));
        assert_eq!(Bt470Bg.code_point(), 5);
    }

    #[test]
    fn to_rgb_sixteenths_of_whole_chroma_equals_to_rgb() {
        for (matrix, range, depth) in CONFIGS {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let max = (1u32 << depth.bits()) - 1;
            for (y, cb, cr) in [(0, 0, 0), (max, max, max), (max / 3, max / 2, max / 5)] {
                let (y, cb, cr) = (y as u16, cb as u16, cr as u16);
                assert_eq!(
                    m.to_rgb_sixteenths(y, 16 * u32::from(cb), 16 * u32::from(cr)),
                    m.to_rgb(y, cb, cr),
                    "{matrix:?} {range:?} {depth:?} ({y}, {cb}, {cr})"
                );
            }
        }
    }

    #[test]
    fn to_rgb_sixteenths_carries_the_fraction_to_the_final_rounding() {
        // 8-bit full-range BT.601: R = Y + 1.402·(Cr - 128). Cr = 128 + 6/16 puts R at Y + 0.526,
        // which rounds up; Cr = 128 + 5/16 puts it at Y + 0.438, which rounds down. Either way
        // rounding the chroma to a whole code first would give Y. B moves the same way through Cb
        // (1.772·(6/16) = 0.665 up, 1.772·(4/16) = 0.443 down), and G = Y - 0.714·(Cr - 128) for
        // Cr = 128 + 12/16 sits at Y - 0.536.
        let m = YcbcrMatrix::new(Bt601, Full, Eight).unwrap();
        assert_eq!(m.to_rgb_sixteenths(100, 2048, 2048 + 6), (101, 100, 100));
        assert_eq!(m.to_rgb_sixteenths(100, 2048, 2048 + 5), (100, 100, 100));
        assert_eq!(m.to_rgb_sixteenths(100, 2048 + 6, 2048), (100, 100, 101));
        assert_eq!(m.to_rgb_sixteenths(100, 2048 + 4, 2048), (100, 100, 100));
        assert_eq!(m.to_rgb_sixteenths(100, 2048, 2048 + 12), (101, 99, 100));
        // Saturation still applies at both ends.
        assert_eq!(m.to_rgb_sixteenths(255, 4080, 4080), (255, 121, 255));
        assert_eq!(m.to_rgb_sixteenths(0, 0, 0), (0, 135, 0));
    }

    #[test]
    fn expand_gray_is_the_luma_only_path() {
        for &(matrix, range, depth) in &CONFIGS {
            let m = YcbcrMatrix::new(matrix, range, depth).unwrap();
            let neutral = anchors(range, depth).2;
            for y in [0, 1, neutral / 3, neutral, depth.max_value()] {
                let gray = m.expand_gray(y);
                assert_eq!(
                    m.to_rgb(y, neutral, neutral),
                    (gray, gray, gray),
                    "{matrix:?} {range:?} {depth:?} y={y}"
                );
            }
        }
    }

    #[test]
    fn expand_gray_full_range_is_the_identity() {
        for depth in [Eight, Ten, Twelve, Sixteen] {
            let m = YcbcrMatrix::monochrome(Full, depth);
            for y in [0, 5, depth.max_value() / 2, depth.max_value()] {
                assert_eq!(m.expand_gray(y), y, "{depth:?} y={y}");
            }
        }
    }

    #[test]
    fn expand_gray_eight_bit_limited_matches_the_studio_swing_formula() {
        // Bit-identity with `((y - 16) * 255 + 109) / 219` clamped, for every 8-bit input — the
        // regression contract for the format crates' existing monochrome expansion.
        let m = YcbcrMatrix::monochrome(Limited, Eight);
        for y in 0..=255u16 {
            let want = (((i32::from(y) - 16) * 255 + 109) / 219).clamp(0, 255) as u16;
            assert_eq!(m.expand_gray(y), want, "y={y}");
        }
    }

    #[test]
    fn expand_gray_clamps_out_of_range_luma() {
        let m = YcbcrMatrix::monochrome(Limited, Ten);
        assert_eq!(m.expand_gray(0), 0);
        assert_eq!(m.expand_gray(1023), 1023);
    }

    #[test]
    fn monochrome_ignores_chroma() {
        let m = YcbcrMatrix::monochrome(Limited, Ten);
        // Proves both chroma coefficients are exactly zero.
        assert_eq!(m.to_rgb(502, 0, 1023), (512, 512, 512));
        assert_eq!(m.to_rgb(502, 1023, 0), (512, 512, 512));
        assert_eq!(m.to_rgb(502, 512, 512), (512, 512, 512));
    }

    #[test]
    fn bit_depth_round_trips() {
        for depth in [Eight, Ten, Twelve, Sixteen] {
            assert_eq!(
                YcbcrMatrix::new(Bt709, Limited, depth).unwrap().bit_depth(),
                depth
            );
            assert_eq!(YcbcrMatrix::monochrome(Full, depth).bit_depth(), depth);
        }
    }

    #[test]
    fn generic_bt601_tracks_the_libwebp_inverse_within_one_lsb() {
        // The documented relationship between the two layers. If someone later "unifies" them,
        // or lets one drift, this fails loudly.
        for range in [Limited, Full] {
            let m = YcbcrMatrix::new(Bt601, range, Eight).unwrap();
            for (y, cb, cr) in [
                (16, 128, 128),
                (235, 128, 128),
                (82, 90, 240),
                (0, 0, 160),
                (128, 128, 128),
                (200, 60, 200),
            ] {
                let want = ycbcr_to_rgb(y, cb, cr, range);
                let got = m.to_rgb(u16::from(y), u16::from(cb), u16::from(cr));
                for (a, b) in [
                    (got.0, u16::from(want.0)),
                    (got.1, u16::from(want.1)),
                    (got.2, u16::from(want.2)),
                ] {
                    assert!(
                        a.abs_diff(b) <= 1,
                        "{range:?} ({y},{cb},{cr}): generic {got:?} vs libwebp {want:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn round_div_rounds_halves_away_from_zero() {
        assert_eq!(round_div(3, 2), 2);
        assert_eq!(round_div(-3, 2), -2);
        assert_eq!(round_div(1, 2), 1);
        assert_eq!(round_div(-1, 2), -1);
        assert_eq!(round_div(2, 3), 1);
        assert_eq!(round_div(-2, 3), -1);
        assert_eq!(round_div(0, 7), 0);
        assert_eq!(round_div(7, 1), 7);
        // Denominators where `2·den` and `2 + den` diverge, on both signs — a denominator of 2
        // makes the two coincide and hides a mutated scale factor.
        assert_eq!(round_div(-7, 3), -2);
        assert_eq!(round_div(7, 3), 2);
        assert_eq!(round_div(-13, 5), -3);
    }

    // ---- RgbToYcbcr -------------------------------------------------------------------------

    #[test]
    fn matrixing_rejects_exactly_what_de_matrixing_rejects() {
        // The two directions must agree on their domain, or a format crate could construct one
        // half of a round trip and fail on the other.
        for mc in [Identity, YCgCo, Unspecified] {
            let error = RgbToYcbcr::new(mc, Limited, Ten).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{mc:?}");
            assert!(
                error.to_string().contains("Kr/Kb matrixing"),
                "{mc:?}: {error}"
            );
            assert!(YcbcrMatrix::new(mc, Limited, Ten).is_err(), "{mc:?}");
        }
        for (mc, range, depth) in CONFIGS {
            assert!(RgbToYcbcr::new(mc, range, depth).is_ok(), "{mc:?}");
        }
    }

    #[test]
    fn full_range_bt601_at_eight_bits_tracks_the_jfif_path_within_one_lsb() {
        // The VP8 layer's full-range arm is an independent hand transcription of full-range BT.601
        // at Q16, so deriving the same matrix from the Table 4 weights pins the derivation formulas
        // against it — but only to 1 LSB, the same bound the module documents for the inverse
        // direction. The transcribed constants are *truncated* (0.587 → 38470, not 38471), so on an
        // exact half like (0, 28, 176) — luma 36.5 — they round down where this rounds up. A
        // mutated weight or a swapped row misses by far more than one code.
        //
        // (Only the *full*-range arm: the limited one is libwebp's truncating Q6 math, which this
        // deliberately does not reproduce.)
        let m = RgbToYcbcr::new(Bt601, Full, Eight).unwrap();
        let (mut differed, mut total) = (0usize, 0usize);
        for r in (0..=255u16).step_by(5) {
            for g in (0..=255u16).step_by(7) {
                for b in (0..=255u16).step_by(11) {
                    let (ey, ecb, ecr) = rgb_to_ycbcr(r as u8, g as u8, b as u8, Full);
                    let got = m.from_rgb(r, g, b);
                    for (got, want, name) in [
                        (got.0, u16::from(ey), "Y"),
                        (got.1, u16::from(ecb), "Cb"),
                        (got.2, u16::from(ecr), "Cr"),
                    ] {
                        assert!(
                            got.abs_diff(want) <= 1,
                            "{name} at ({r},{g},{b}): {got} vs {want}"
                        );
                        differed += usize::from(got != want);
                        total += 1;
                    }
                }
            }
        }
        // Ties are rare: 21 of 138 528 samples on this grid. Without a density bound a derivation
        // that merely *looked* like BT.601 could sit 1 LSB off everywhere and still pass the
        // per-sample check above.
        assert!(
            differed * 1000 < total,
            "{differed} of {total} samples differed — expected well under 0.1%"
        );
    }

    #[test]
    fn primaries_land_on_the_h273_reference_values() {
        // Hand-computed from H.273 §8.3 with the Table 4 weights. Pure red under BT.709
        // (Kr = 0.2126) has Y = round(255·0.2126) = 54, Cr at its positive extreme (255) and Cb at
        // its negative one: Cb = round(255·(0 − 0.2126)/(2·0.9278)) + 128 = round(−29.2) + 128 = 99.
        let m709 = RgbToYcbcr::new(Bt709, Full, Eight).unwrap();
        assert_eq!(m709.from_rgb(255, 0, 0), (54, 99, 255));
        // Pure blue: Y = round(255·0.0722) = 18, Cb at +255, Cr = round(255·(0 − 0.0722)/(2·0.7874))
        // + 128 = round(−11.7) + 128 = 116.
        assert_eq!(m709.from_rgb(0, 0, 255), (18, 255, 116));

        // BT.601 (Kr = 0.299): pure red → Y = round(255·0.299) = 76.
        let m601 = RgbToYcbcr::new(Bt601, Full, Eight).unwrap();
        assert_eq!(m601.from_rgb(255, 0, 0).0, 76);
        // BT.2020 (Kr = 0.2627): pure red → Y = round(255·0.2627) = 67.
        let m2020 = RgbToYcbcr::new(Bt2020Ncl, Full, Eight).unwrap();
        assert_eq!(m2020.from_rgb(255, 0, 0).0, 67);
        // The three matrices really are distinct — a mutated weight table that collapsed them
        // would still pass a round-trip test.
        assert_ne!(m709.from_rgb(255, 0, 0), m601.from_rgb(255, 0, 0));
        assert_ne!(m709.from_rgb(255, 0, 0), m2020.from_rgb(255, 0, 0));
        // BT.470 B,G names the same de-matrixing as BT.601 in this direction too.
        assert_eq!(
            RgbToYcbcr::new(Bt470Bg, Full, Eight).unwrap(),
            RgbToYcbcr::new(Bt601, Full, Eight).unwrap()
        );
    }

    #[test]
    fn range_anchors_hold_at_every_depth() {
        for (mc, range, depth) in CONFIGS {
            let m = RgbToYcbcr::new(mc, range, depth).unwrap();
            let bits = u32::from(depth.bits());
            let max = (1u32 << bits) - 1;
            let (black, white, centre) = match range {
                // Studio swing scales the 16/235/128 anchors by `1 << (bits - 8)`.
                Limited => {
                    let s = 1u32 << (bits - 8);
                    (16 * s, 235 * s, 128 * s)
                }
                Full => (0, max, 1u32 << (bits - 1)),
            };
            let max = max as u16;
            assert_eq!(
                m.from_rgb(0, 0, 0),
                (black as u16, centre as u16, centre as u16),
                "{mc:?} {range:?} {depth:?} black"
            );
            assert_eq!(
                m.from_rgb(max, max, max),
                (white as u16, centre as u16, centre as u16),
                "{mc:?} {range:?} {depth:?} white"
            );
            assert_eq!(m.bit_depth(), depth, "{mc:?} {range:?} {depth:?}");
        }
    }

    #[test]
    fn matrixing_round_trips_through_de_matrixing() {
        // YCbCr at the same depth is not a bijection of RGB — the transform is lossy by
        // quantization alone. Bound the error rather than assert equality, over a grid dense
        // enough to catch a sign flip or a swapped row (either blows past the tolerance at once).
        for (mc, range, depth) in CONFIGS {
            let fwd = RgbToYcbcr::new(mc, range, depth).unwrap();
            let inv = YcbcrMatrix::new(mc, range, depth).unwrap();
            let bits = u32::from(depth.bits());
            let max = ((1u32 << bits) - 1) as u16;
            // Studio swing spends only 219/256 of the luma codes, so its round trip is looser.
            let tolerance = match range {
                Full => max / 255 + 1,
                Limited => 2 * (max / 255 + 1),
            };
            let step = (usize::from(max) / 12).max(1);
            for r in (0..=usize::from(max)).step_by(step) {
                for g in (0..=usize::from(max)).step_by(step + 1) {
                    for b in (0..=usize::from(max)).step_by(step + 2) {
                        let (r, g, b) = (r as u16, g as u16, b as u16);
                        let (y, cb, cr) = fwd.from_rgb(r, g, b);
                        let (r2, g2, b2) = inv.to_rgb(y, cb, cr);
                        for (src, back, name) in [(r, r2, "R"), (g, g2, "G"), (b, b2, "B")] {
                            assert!(
                                src.abs_diff(back) <= tolerance,
                                "{mc:?} {range:?} {depth:?} {name} at ({r},{g},{b}): \
                                 {src} → {back} (tolerance {tolerance})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn out_of_range_rgb_saturates_instead_of_wrapping() {
        // A caller can hand an 8-bit matrix a sample above 255 (a widened 10-bit plane, say); the
        // result must clamp to the depth, not wrap through the `as u16` narrowing.
        let m = RgbToYcbcr::new(Bt709, Full, Eight).unwrap();
        assert_eq!(m.from_rgb(4095, 0, 0), (255, 0, 255));
        assert_eq!(m.from_rgb(0, 4095, 0).0, 255);
    }
}
