//! Planar-to-RGBA presentation: the colour policy, the sample scale, alpha, the transformative
//! properties and `iovl` compositing, all over interleaved RGBA at the surface's sample width.
//!
//! Every function that can refuse takes the binding's `origin`, so a refusal names the format the
//! caller decoded rather than this shared layer.

use gamut_color::{
    BitDepth, ChromaSubsampling, ChromaUpsampling, ColorRange, MatrixCoefficients, YcbcrMatrix,
    bt601_ycbcr_to_rgb,
};
use gamut_core::{Error, Result};
use gamut_dsp::upsample::{libavif, taps};
use gamut_isobmff::CleanAperture;

use crate::codestream::AlphaChroma;
use crate::frame::DecodedFrame;
use crate::image::TransformativeProperty;
use crate::limits::try_filled;

// ---- presentation sample width ---------------------------------------------------------------

mod sample_sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
}

/// The interleaved-RGBA sample type of a presentation surface: `u8` for `Rgba8`, `u16` for
/// `Rgba16`. Sealed — those two surfaces are the only implementors.
///
/// Every presentation surface carries samples over its type's **full** range, so [`Self::MAX`] is
/// both the opaque-alpha value and the unity of the source-over blend.
pub(crate) trait RgbaSample: sample_sealed::Sealed + Copy + Default + Eq {
    /// The all-ones sample: `255` for `u8`, `65535` for `u16`.
    const MAX: u32;

    /// The deepest coded frame this surface will present. The `u8` surface stops at 8 so that
    /// narrowing a 10/12-bit frame is an explicit
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error rather than silent
    /// quality loss; the `u16` surface accepts everything [`DecodedFrame`] can carry.
    const MAX_CODED_DEPTH: u8;

    /// The refusal message for a frame deeper than [`Self::MAX_CODED_DEPTH`].
    const TOO_DEEP: &'static str;

    /// Narrows a value the caller has already bounded by [`Self::MAX`].
    fn narrow(v: u32) -> Self;

    /// Widens a sample for arithmetic.
    fn widen(self) -> u32;

    /// Narrows one `iovl` `canvas_fill_value` channel to this sample width by taking the **most
    /// significant bits** (ISO/IEC 23008-12 §6.6.2.4): `v >> 8` for `u8`, `v` unchanged for `u16`.
    fn from_canvas_fill(v: u16) -> Self;
}

impl RgbaSample for u8 {
    const MAX: u32 = 255;
    const MAX_CODED_DEPTH: u8 = 8;
    const TOO_DEEP: &'static str =
        "HEIF: >8-bit RGBA presentation needs the 16-bit surface (or the planar surface)";

    fn narrow(v: u32) -> Self {
        v as Self
    }

    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn from_canvas_fill(v: u16) -> Self {
        (v >> 8) as Self
    }
}

impl RgbaSample for u16 {
    const MAX: u32 = 65535;
    const MAX_CODED_DEPTH: u8 = 16;
    const TOO_DEEP: &'static str = "HEIF: bit depth is out of range (use the planar surface)";

    fn narrow(v: u32) -> Self {
        v as Self
    }

    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn from_canvas_fill(v: u16) -> Self {
        v
    }
}

/// Rescales one coded sample from a plane whose white level is `max_in` onto the presentation
/// sample's full range, round-to-nearest, saturating a sample that exceeds its declared depth.
///
/// This is the single place the surfaces' sample-scale policy lives. `rescale::<u8>(s, 255)` is
/// the identity, and `rescale::<u16>(s, 255)` is exactly `s * 257` — so widening 8-bit content to
/// the 16-bit surface is lossless.
///
/// Round-to-nearest rather than bit replication: the two differ (at 10-bit, sample 15 gives 961
/// here and 960 replicated).
pub(crate) fn rescale<T: RgbaSample>(s: u16, max_in: u32) -> T {
    let s = u64::from(u32::from(s).min(max_in));
    let max = u64::from(max_in);
    T::narrow(((s * u64::from(T::MAX) + max / 2) / max) as u32)
}

// ---- colour conversion -----------------------------------------------------------------------

/// The per-frame YCbCr → RGB conversion the colour policy selected.
enum Converter {
    /// 8-bit BT.601 through [`bt601_ycbcr_to_rgb`], gamut-color's libwebp-exact integer inverse —
    /// the conversion both codec crates have always used at 8 bits, kept so shipped output stays
    /// byte-identical.
    Bt601Legacy8(ColorRange),
    /// The bit-depth- and matrix-generic H.273 §8.3 de-matrixing.
    Generic(YcbcrMatrix),
}

impl Converter {
    /// The converter for a `matrix_coefficients` code point at `range` and `bit_depth`.
    ///
    /// Matrix 0 (identity / GBR) never reaches here — it is a plane permutation, handled inline.
    fn select(matrix: u16, range: ColorRange, bit_depth: u8, origin: &'static str) -> Result<Self> {
        // BT.601 at 8 bits keeps the libwebp-exact inverse; every other pair takes the generic
        // path.
        if matches!(matrix, 2 | 5 | 6) && bit_depth == 8 {
            return Ok(Converter::Bt601Legacy8(range));
        }
        generic_matrix(matrix, range, bit_depth, origin).map(Converter::Generic)
    }

    /// Converts one triple, returning samples at the **input** depth's scale, so the caller
    /// rescales from the frame's white level either way.
    fn to_rgb(&self, y: u16, cb: u16, cr: u16) -> (u16, u16, u16) {
        match self {
            Converter::Bt601Legacy8(range) => {
                let (r, g, b) = bt601_ycbcr_to_rgb(y as u8, cb as u8, cr as u8, *range);
                (u16::from(r), u16::from(g), u16::from(b))
            }
            Converter::Generic(matrix) => matrix.to_rgb(y, cb, cr),
        }
    }
}

/// The H.273 de-matrixing for a `matrix_coefficients` code point at `range` and `bit_depth`, or
/// an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the RGBA surfaces
/// do not present that combination. Matrix 2 (unspecified) resolves to BT.601, the default
/// libavif and libheif both assume.
fn generic_matrix(
    matrix: u16,
    range: ColorRange,
    bit_depth: u8,
    origin: &'static str,
) -> Result<YcbcrMatrix> {
    let unsupported = || {
        Error::unsupported(
            origin,
            "HEIF: unsupported matrix coefficients or bit depth on the RGBA surface (BT.709, \
             BT.601/BT.470 B,G, BT.2020 NCL, identity, and monochrome are supported at 8, 10, 12 \
             and 16 bits)",
        )
    };
    let coefficients = if matrix == 2 {
        MatrixCoefficients::Bt601
    } else {
        MatrixCoefficients::from_code_point(matrix).ok_or_else(unsupported)?
    };
    // `DecodedFrame` admits any depth in 8..=16, but only the CICP-modeled depths have a
    // de-matrixing; an unmodeled depth is an explicit refusal, never a silent reinterpretation.
    let depth = BitDepth::from_bits(u32::from(bit_depth)).ok_or_else(unsupported)?;
    YcbcrMatrix::new(coefficients, range, depth)
}

/// Converts a planar [`DecodedFrame`] to an interleaved RGBA buffer (opaque alpha) at the
/// presentation sample width `T`, de-matrixing with `matrix` at `range` and reconstructing
/// subsampled chroma with `upsampling`.
pub(crate) fn frame_to_rgba<T: RgbaSample>(
    frame: &DecodedFrame,
    (matrix, range): (u16, ColorRange),
    upsampling: ChromaUpsampling,
    origin: &'static str,
) -> Result<Vec<T>> {
    let depth = frame.bit_depth();
    if depth > T::MAX_CODED_DEPTH {
        return Err(Error::unsupported(origin, T::TOO_DEEP));
    }
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let max_in = (1u32 << depth) - 1;
    let opaque = T::narrow(T::MAX);
    let len = w
        .checked_mul(h)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| Error::invalid_input(origin, "HEIF: image is too large to present"))?;
    let mut out = try_filled(len, T::default(), origin)?;
    let (luma, cb_plane, cr_plane) = (frame.y(), frame.cb(), frame.cr());
    let chroma = frame.chroma();

    if chroma == ChromaSubsampling::Cs400 {
        for (px, &y) in out.as_chunks_mut::<4>().0.iter_mut().zip(luma) {
            let g = expand_gray::<T>(y, depth, range);
            *px = [g, g, g, opaque];
        }
        return Ok(out);
    }

    // Identity / GBR (mc = 0): plane order Y=G, Cb=B, Cr=R; requires 4:4:4, no range expansion.
    if matrix == 0 {
        if chroma != ChromaSubsampling::Cs444 {
            return Err(Error::unsupported(
                origin,
                "HEIF: identity matrix (mc = 0) requires 4:4:4 chroma",
            ));
        }
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [
                rescale::<T>(cr_plane[i], max_in),
                rescale::<T>(luma[i], max_in),
                rescale::<T>(cb_plane[i], max_in),
                opaque,
            ];
        }
        return Ok(out);
    }

    let (cw, ch) = frame.chroma_dimensions();
    let (cw, ch) = (cw as usize, ch as usize);
    // libavif's bilinear upsampling runs on 4:2:0 (both axes) and 4:2:2 (along the row); every
    // other layout, and `Nearest` everywhere, replicates each chroma sample.
    let interpolate_rows = match (upsampling, chroma) {
        (ChromaUpsampling::Interpolated, ChromaSubsampling::Cs420) => Some(true),
        (ChromaUpsampling::Interpolated, ChromaSubsampling::Cs422) => Some(false),
        _ => None,
    };
    if let Some(vertical) = interpolate_rows {
        // Kept in sixteenths through the conversion.
        let matrix = generic_matrix(matrix, range, depth, origin)?;
        let (mut cb, mut cr) = (vec![0u32; w], vec![0u32; w]);
        for (y, line) in out.chunks_exact_mut(w * 4).enumerate() {
            let (near, far) = if vertical { taps(y, ch) } else { (y, y) };
            let (n, f) = (near * cw..(near + 1) * cw, far * cw..(far + 1) * cw);
            libavif::bilinear_row(&cb_plane[n.clone()], &cb_plane[f.clone()], &mut cb);
            libavif::bilinear_row(&cr_plane[n], &cr_plane[f], &mut cr);
            let row = &luma[y * w..(y + 1) * w];
            for (((px, &yy), &cb), &cr) in line
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row)
                .zip(&cb)
                .zip(&cr)
            {
                let (r, g, b) = matrix.to_rgb_sixteenths(yy, cb, cr);
                *px = [
                    rescale::<T>(r, max_in),
                    rescale::<T>(g, max_in),
                    rescale::<T>(b, max_in),
                    opaque,
                ];
            }
        }
        return Ok(out);
    }

    let converter = Converter::select(matrix, range, depth, origin)?;
    // Each factor is 1 or 2, so its base-2 logarithm is the shift from a luma to a chroma index.
    let factors = chroma.factors();
    let (shift_x, shift_y) = (
        factors.width().trailing_zeros(),
        factors.height().trailing_zeros(),
    );
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % w, i / w);
        let ci = (y >> shift_y) * cw + (x >> shift_x);
        let (r, g, b) = converter.to_rgb(luma[i], cb_plane[ci], cr_plane[ci]);
        *px = [
            rescale::<T>(r, max_in),
            rescale::<T>(g, max_in),
            rescale::<T>(b, max_in),
            opaque,
        ];
    }
    Ok(out)
}

/// Expands one monochrome luma sample at `bit_depth` to a display gray value on the presentation
/// sample's full range: the identity for full range, and the studio-swing expansion
/// `(y - 16·2^(bit_depth-8)) · max / (219·2^(bit_depth-8))` (rounded, clamped) for limited range.
///
/// The expansion happens **at the coded depth** and the result is then rescaled, so one
/// precision model covers the whole surface and 8-bit content widens onto the 16-bit surface
/// exactly. At `bit_depth = 8` the limited-range arm is exactly `((y - 16)·255 + 109)/219`, since
/// `219/2 == 109` in integer division.
pub(crate) fn expand_gray<T: RgbaSample>(y: u16, bit_depth: u8, range: ColorRange) -> T {
    let max_in = (1u32 << bit_depth) - 1;
    let coded = match range {
        ColorRange::Full => u32::from(y).min(max_in),
        ColorRange::Limited => {
            let s = i64::from(1u32 << (bit_depth - 8));
            let black = 16 * s;
            let span = 219 * s;
            let max = i64::from(max_in);
            let expanded = ((i64::from(y) - black) * max + span / 2) / span;
            expanded.clamp(0, max) as u32
        }
    };
    rescale::<T>(coded as u16, max_in)
}

/// Merges an alpha auxiliary's luma plane into the RGBA buffer's alpha channel, rescaling from
/// the auxiliary's own bit depth (independent of the master's) to the presentation sample's full
/// range. `policy` says whether a coloured auxiliary is accepted; the dimensions must match the
/// master's pre-transform `w × h`.
pub(crate) fn apply_alpha<T: RgbaSample>(
    rgba: &mut [T],
    (w, h): (u32, u32),
    alpha: &DecodedFrame,
    policy: AlphaChroma,
    origin: &'static str,
) -> Result<()> {
    if policy == AlphaChroma::Monochrome && alpha.chroma() != ChromaSubsampling::Cs400 {
        return Err(Error::invalid_input(
            origin,
            "HEIF: alpha auxiliary must be monochrome",
        ));
    }
    if (alpha.width(), alpha.height()) != (w, h) {
        return Err(Error::invalid_input(
            origin,
            "HEIF: alpha auxiliary dimensions do not match the master image",
        ));
    }
    let max = (1u32 << alpha.bit_depth()) - 1;
    for (px, &a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(alpha.y()) {
        // Depth 8 onto the `u8` surface needs no special case: `(s * 255 + 127) / 255 == s` for
        // every `s` in `0..=255`, so the general rescale is already the identity there.
        px[3] = rescale::<T>(a, max);
    }
    Ok(())
}

// ---- transformative properties ---------------------------------------------------------------

/// An interleaved RGBA buffer and its dimensions.
pub(crate) type Rgba<T> = (Vec<T>, u32, u32);

/// Applies transformative properties to an interleaved RGBA buffer in `ipma` (list) order,
/// returning the transformed buffer and its dimensions.
pub(crate) fn apply_transforms<T: Copy + Default>(
    props: Vec<TransformativeProperty>,
    rgba: Rgba<T>,
    origin: &'static str,
) -> Result<Rgba<T>> {
    let mut current = rgba;
    for tp in props {
        current = match tp {
            // `turns` is 0..=3 by construction — the isobmff `irot` parser masks the angle field
            // with `& 0x03` — so each unit is exactly one 90° anti-clockwise rotation.
            TransformativeProperty::Rotation(turns) => {
                (0..turns).try_fold(current, |(px, w, h), _| rotate90_ccw(&px, w, h, origin))?
            }
            TransformativeProperty::Mirror(axis) => {
                let (px, w, h) = current;
                (mirror(&px, w, h, axis, origin)?, w, h)
            }
            TransformativeProperty::CleanAperture(clap) => {
                let (px, w, h) = current;
                crop_clap(&px, w, h, &clap, origin)?
            }
        };
    }
    Ok(current)
}

/// Rotates an interleaved RGBA buffer 90° anti-clockwise: output `(ox, oy)` reads input
/// `(w - 1 - oy, ox)`, giving output dimensions `(h, w)`.
pub(crate) fn rotate90_ccw<T: Copy + Default>(
    src: &[T],
    w: u32,
    h: u32,
    origin: &'static str,
) -> Result<Rgba<T>> {
    let (wu, hu) = (w as usize, h as usize);
    let (nw, nh) = (hu, wu);
    let mut out = try_filled(nw * nh * 4, T::default(), origin)?;
    for oy in 0..nh {
        for ox in 0..nw {
            let (sx, sy) = (wu - 1 - oy, ox);
            let si = (sy * wu + sx) * 4;
            let oi = (oy * nw + ox) * 4;
            out[oi..oi + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Ok((out, h, w))
}

/// Mirrors an interleaved RGBA buffer per the `imir` axis semantics of ISO/IEC 23008-12:2022
/// §6.5.12: `axis = 1` exchanges the left and right parts, any other value (the field is one bit,
/// so `0`) the top and bottom parts — the reading libheif and libavif implement. Dimensions are
/// unchanged.
pub(crate) fn mirror<T: Copy + Default>(
    src: &[T],
    w: u32,
    h: u32,
    axis: u8,
    origin: &'static str,
) -> Result<Vec<T>> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = try_filled(wu * hu * 4, T::default(), origin)?;
    for y in 0..hu {
        for x in 0..wu {
            let (sx, sy) = if axis == 1 {
                (wu - 1 - x, y)
            } else {
                (x, hu - 1 - y)
            };
            let si = (sy * wu + sx) * 4;
            let oi = (y * wu + x) * 4;
            out[oi..oi + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Ok(out)
}

/// Crops an interleaved RGBA buffer to its `clap` clean aperture (ISO/IEC 14496-12 §12.1.4).
///
/// The clean aperture is a `cleanApertureWidth × cleanApertureHeight` rectangle centred at
/// `((w - 1)/2 + horizOff, (h - 1)/2 + vertOff)`, so its top-left corner is
/// `left = (w - cleanApertureWidth)/2 + horizOff` and
/// `top = (h - cleanApertureHeight)/2 + vertOff` (the width/height and offset centre terms
/// combine exactly, matching libavif's integer clap conversion). The crop must be integer-valued
/// and lie fully inside the image; otherwise it is rejected with an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error.
pub(crate) fn crop_clap<T: Copy + Default>(
    src: &[T],
    w: u32,
    h: u32,
    clap: &CleanAperture,
    origin: &'static str,
) -> Result<Rgba<T>> {
    let refuse = |message| Error::invalid_input(origin, message);
    if clap.width_d == 0 || clap.height_d == 0 || clap.horiz_off_d == 0 || clap.vert_off_d == 0 {
        return Err(refuse("HEIF: clap has a zero denominator"));
    }
    if !clap.width_n.is_multiple_of(clap.width_d) || !clap.height_n.is_multiple_of(clap.height_d) {
        return Err(refuse("HEIF: clap width/height is not integer-valued"));
    }
    let crop_w = clap.width_n / clap.width_d;
    let crop_h = clap.height_n / clap.height_d;
    if crop_w == 0 || crop_h == 0 {
        return Err(refuse("HEIF: clap has a zero-sized crop"));
    }
    let left = clap_offset(
        i64::from(w),
        i64::from(crop_w),
        clap.horiz_off_n,
        clap.horiz_off_d,
    )
    .ok_or_else(|| refuse("HEIF: clap offset is not integer-valued"))?;
    let top = clap_offset(
        i64::from(h),
        i64::from(crop_h),
        clap.vert_off_n,
        clap.vert_off_d,
    )
    .ok_or_else(|| refuse("HEIF: clap offset is not integer-valued"))?;
    if left < 0
        || top < 0
        || left + i64::from(crop_w) > i64::from(w)
        || top + i64::from(crop_h) > i64::from(h)
    {
        return Err(refuse("HEIF: clap crop window lies outside the image"));
    }

    let (wu, left, top) = (w as usize, left as usize, top as usize);
    let (cw, chh) = (crop_w as usize, crop_h as usize);
    let mut out = try_filled(cw * chh * 4, T::default(), origin)?;
    for y in 0..chh {
        let si = ((top + y) * wu + left) * 4;
        out[y * cw * 4..(y + 1) * cw * 4].copy_from_slice(&src[si..si + cw * 4]);
    }
    Ok((out, crop_w, crop_h))
}

/// The integer top-left offset of a clean-aperture crop along one axis:
/// `left = (dim - crop)/2 + off_n/off_d`, evaluated exactly as
/// `((dim - crop)·off_d + 2·off_n) / (2·off_d)`, or `None` when that is not an integer. `off_n`
/// is signed (two's-complement bits); `off_d` is a positive denominator.
fn clap_offset(dim: i64, crop: i64, off_n: u32, off_d: u32) -> Option<i64> {
    let num = (dim - crop) * i64::from(off_d) + 2 * i64::from(off_n as i32);
    let den = 2 * i64::from(off_d);
    (num % den == 0).then_some(num / den)
}

// ---- overlay compositing ---------------------------------------------------------------------

/// Composites a source RGBA image over the canvas at signed offset `(dx, dy)`, clipping to the
/// canvas, with unassociated-alpha source-over blending.
///
/// Source-over in integer math, with `MAX` the presentation sample's all-ones value (`sa`/`da`
/// the source/destination alpha, `sc`/`dc` a channel): `out_a = sa + round(da·(MAX - sa)/MAX)`,
/// and each channel `out_c = round((sc·sa + round(dc·da·(MAX - sa)/MAX)) / out_a)`, or a fully
/// transparent pixel when `out_a = 0`. Rounding is round-half-up (`+ half`).
pub(crate) fn composite_over<T: RgbaSample>(
    canvas: &mut [T],
    (cw, ch): (u32, u32),
    src: &[T],
    (sw, sh): (u32, u32),
    (dx, dy): (i32, i32),
) {
    // The intermediates are `u64`: at 16-bit samples `dc·da·inv` reaches ~2.8e14, far past `u32`.
    let max = u64::from(T::MAX);
    let transparent = T::narrow(0);
    for sy in 0..i64::from(sh) {
        let y = i64::from(dy) + sy;
        if y < 0 || y >= i64::from(ch) {
            continue;
        }
        for sx in 0..i64::from(sw) {
            let x = i64::from(dx) + sx;
            if x < 0 || x >= i64::from(cw) {
                continue;
            }
            let si = ((sy * i64::from(sw) + sx) * 4) as usize;
            let di = ((y * i64::from(cw) + x) * 4) as usize;
            let sa = u64::from(src[si + 3].widen());
            let inv = max - sa;
            let da = u64::from(canvas[di + 3].widen());
            let da_contrib = (da * inv + max / 2) / max;
            let out_a = sa + da_contrib;
            if out_a == 0 {
                canvas[di..di + 4].copy_from_slice(&[transparent; 4]);
                continue;
            }
            for c in 0..3 {
                let sc = u64::from(src[si + c].widen());
                let dc = u64::from(canvas[di + c].widen());
                let num = sc * sa + (dc * da * inv + max / 2) / max;
                canvas[di + c] = T::narrow(((num + out_a / 2) / out_a).min(max) as u32);
            }
            canvas[di + 3] = T::narrow(out_a as u32);
        }
    }
}
