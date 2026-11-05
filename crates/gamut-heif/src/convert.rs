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
    // YCgCo and the other non-Kr/Kb code points parse but have no de-matrixing: the same
    // refusal, under the caller's origin rather than gamut-color's.
    YcbcrMatrix::new(coefficients, range, depth).map_err(|_| unsupported())
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

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, ErrorKind};

    use super::*;

    const ORIGIN: &str = "gamut-caller";

    /// A frame of `w × h` at `depth` and `chroma` from explicit planes.
    fn frame(
        (w, h): (u32, u32),
        depth: u8,
        chroma: ChromaSubsampling,
        planes: [Vec<u16>; 3],
    ) -> DecodedFrame {
        let [y, cb, cr] = planes;
        DecodedFrame::new(Dimensions::new(w, h).unwrap(), depth, chroma, y, cb, cr).unwrap()
    }

    fn refusal<T: core::fmt::Debug>(result: Result<T>) -> (ErrorKind, Option<&'static str>) {
        let error = result.unwrap_err();
        assert_eq!(error.origin(), Some(ORIGIN));
        (error.kind(), error.static_message())
    }

    fn to_rgba8(
        f: &DecodedFrame,
        colour: (u16, ColorRange),
        up: ChromaUpsampling,
    ) -> Result<Vec<u8>> {
        frame_to_rgba::<u8>(f, colour, up, ORIGIN)
    }

    #[test]
    fn rescale_rounds_to_nearest_and_saturates() {
        assert_eq!(rescale::<u8>(200, 255), 200);
        assert_eq!(rescale::<u16>(200, 255), 200 * 257);
        // (15·65535 + 511) / 1023 = 961.4…; bit replication would give 960.
        assert_eq!(rescale::<u16>(15, 1023), 961);
        assert_eq!(rescale::<u8>(512, 1023), 128);
        assert_eq!(rescale::<u8>(300, 255), 255);
    }

    #[test]
    fn a_canvas_fill_keeps_its_most_significant_bits() {
        assert_eq!(u8::from_canvas_fill(0x12FF), 0x12);
        assert_eq!(u16::from_canvas_fill(0x12FF), 0x12FF);
    }

    #[test]
    fn studio_swing_gray_expands_at_its_coded_depth() {
        let limited = |y, depth| expand_gray::<u16>(y, depth, ColorRange::Limited);
        assert_eq!(expand_gray::<u8>(16, 8, ColorRange::Limited), 0);
        assert_eq!(expand_gray::<u8>(235, 8, ColorRange::Limited), 255);
        // ((126 - 16)·255 + 109) / 219 = 128.6… → 128.
        assert_eq!(expand_gray::<u8>(126, 8, ColorRange::Limited), 128);
        assert_eq!(expand_gray::<u8>(250, 8, ColorRange::Limited), 255);
        assert_eq!(expand_gray::<u8>(3, 8, ColorRange::Limited), 0);
        assert_eq!(limited(64, 10), 0);
        assert_eq!(limited(940, 10), 65535);
        assert_eq!(expand_gray::<u8>(77, 8, ColorRange::Full), 77);
        assert_eq!(expand_gray::<u16>(1023, 10, ColorRange::Full), 65535);
    }

    #[test]
    fn bt601_at_8_bits_takes_the_libwebp_exact_inverse() {
        // One pixel per row: black, white, and a saturated red at limited range.
        let f = frame(
            (1, 3),
            8,
            ChromaSubsampling::Cs444,
            [vec![16, 235, 81], vec![128, 128, 90], vec![128, 128, 240]],
        );
        let rgba = to_rgba8(&f, (6, ColorRange::Limited), ChromaUpsampling::Nearest).unwrap();
        let expected: Vec<u8> = [(16, 128, 128), (235, 128, 128), (81, 90, 240)]
            .into_iter()
            .flat_map(|(y, cb, cr)| {
                let (r, g, b) = bt601_ycbcr_to_rgb(y, cb, cr, ColorRange::Limited);
                [r, g, b, 255]
            })
            .collect();
        assert_eq!(rgba, expected);
        assert_eq!(&rgba[..8], &[0, 0, 0, 255, 255, 255, 255, 255]);
        // Matrices 2 (unspecified) and 5 present exactly as 6.
        for matrix in [2, 5] {
            assert_eq!(
                to_rgba8(&f, (matrix, ColorRange::Limited), ChromaUpsampling::Nearest).unwrap(),
                rgba
            );
        }
    }

    #[test]
    fn deeper_frames_take_the_generic_matrix_and_unspecified_is_bt601() {
        let f = frame(
            (1, 2),
            10,
            ChromaSubsampling::Cs444,
            [vec![1023, 300], vec![512, 700], vec![512, 200]],
        );
        let rgba16 = |matrix| {
            frame_to_rgba::<u16>(
                &f,
                (matrix, ColorRange::Full),
                ChromaUpsampling::Nearest,
                ORIGIN,
            )
            .unwrap()
        };
        // Full-range white stays white at the 16-bit scale.
        assert_eq!(&rgba16(1)[..4], &[65535; 4]);
        let bt601 = YcbcrMatrix::new(MatrixCoefficients::Bt601, ColorRange::Full, BitDepth::Ten)
            .unwrap()
            .to_rgb(300, 700, 200);
        let expected = [bt601.0, bt601.1, bt601.2].map(|s| rescale::<u16>(s, 1023));
        assert_eq!(&rgba16(2)[4..7], &expected);
        assert_eq!(rgba16(2), rgba16(6));
        assert_ne!(rgba16(1), rgba16(6));
    }

    #[test]
    fn unpresentable_colour_is_refused_with_the_callers_origin() {
        let f = |depth| {
            frame(
                (1, 1),
                depth,
                ChromaSubsampling::Cs444,
                [vec![0], vec![0], vec![0]],
            )
        };
        let unsupported = |matrix, depth| {
            refusal(to_rgba8(
                &f(depth),
                (matrix, ColorRange::Full),
                ChromaUpsampling::Nearest,
            ))
            .0
        };
        // YCgCo (8), a chromaticity-derived matrix (12), a reserved code point (3).
        for matrix in [8, 12, 3] {
            assert_eq!(unsupported(matrix, 8), ErrorKind::Unsupported, "{matrix}");
        }
        assert_eq!(
            refusal(to_rgba8(
                &f(10),
                (1, ColorRange::Full),
                ChromaUpsampling::Nearest
            )),
            (ErrorKind::Unsupported, Some(u8::TOO_DEEP))
        );
        // 9 bits is a depth `DecodedFrame` admits but CICP does not model.
        let nine = frame_to_rgba::<u16>(
            &f(9),
            (1, ColorRange::Full),
            ChromaUpsampling::Nearest,
            ORIGIN,
        );
        assert_eq!(refusal(nine).0, ErrorKind::Unsupported);
    }

    #[test]
    fn identity_maps_planes_to_gbr_and_needs_4_4_4() {
        let f = frame(
            (1, 1),
            8,
            ChromaSubsampling::Cs444,
            [vec![10], vec![20], vec![30]],
        );
        let rgba = to_rgba8(&f, (0, ColorRange::Limited), ChromaUpsampling::Nearest).unwrap();
        assert_eq!(rgba, [30, 10, 20, 255]);
        let f = frame(
            (2, 1),
            8,
            ChromaSubsampling::Cs422,
            [vec![10, 11], vec![20], vec![30]],
        );
        assert_eq!(
            refusal(to_rgba8(
                &f,
                (0, ColorRange::Full),
                ChromaUpsampling::Nearest
            )),
            (
                ErrorKind::Unsupported,
                Some("HEIF: identity matrix (mc = 0) requires 4:4:4 chroma")
            )
        );
    }

    #[test]
    fn monochrome_replicates_expanded_luma() {
        let f = frame(
            (2, 1),
            8,
            ChromaSubsampling::Cs400,
            [vec![16, 235], vec![], vec![]],
        );
        // The matrix is irrelevant to a monochrome frame, even an unpresentable one.
        let rgba = to_rgba8(&f, (8, ColorRange::Limited), ChromaUpsampling::Nearest).unwrap();
        assert_eq!(rgba, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn nearest_upsampling_indexes_chroma_by_each_axis_factor() {
        // 2×2 luma at 4:4:0: one chroma row, two columns. Column x reads chroma[x] on both rows.
        let f = frame(
            (2, 2),
            8,
            ChromaSubsampling::Cs440,
            [vec![128; 4], vec![60, 200], vec![128, 128]],
        );
        let rgba = to_rgba8(&f, (6, ColorRange::Full), ChromaUpsampling::Nearest).unwrap();
        let px = |i: usize| &rgba[i * 4..i * 4 + 3];
        assert_eq!(px(0), px(2));
        assert_eq!(px(1), px(3));
        assert_ne!(px(0), px(1));
        // 2×2 at 4:2:2: one chroma column per row; row y reads chroma[y] on both columns.
        let f = frame(
            (2, 2),
            8,
            ChromaSubsampling::Cs422,
            [vec![128; 4], vec![60, 200], vec![128, 128]],
        );
        let rgba = to_rgba8(&f, (6, ColorRange::Full), ChromaUpsampling::Nearest).unwrap();
        let px = |i: usize| &rgba[i * 4..i * 4 + 3];
        assert_eq!(px(0), px(1));
        assert_eq!(px(2), px(3));
        assert_ne!(px(0), px(2));
    }

    #[test]
    fn interpolated_upsampling_blends_subsampled_chroma_only() {
        // 4×1 at 4:2:2 with two very different chroma samples: replication keeps pairs equal,
        // the bilinear filter does not.
        let f = frame(
            (4, 1),
            8,
            ChromaSubsampling::Cs422,
            [vec![128; 4], vec![16, 240], vec![128, 128]],
        );
        let colour = (6, ColorRange::Full);
        let nearest = to_rgba8(&f, colour, ChromaUpsampling::Nearest).unwrap();
        let interpolated = to_rgba8(&f, colour, ChromaUpsampling::Interpolated).unwrap();
        assert_eq!(nearest[4..8], nearest[8 - 4..8]);
        assert_eq!(nearest[0..4], nearest[4..8]);
        assert_ne!(interpolated[4..8], nearest[4..8]);
        // At 4:4:4 there is nothing to interpolate.
        let f = frame(
            (2, 1),
            8,
            ChromaSubsampling::Cs444,
            [vec![128; 2], vec![16, 240], vec![128, 128]],
        );
        assert_eq!(
            to_rgba8(&f, colour, ChromaUpsampling::Interpolated).unwrap(),
            to_rgba8(&f, colour, ChromaUpsampling::Nearest).unwrap()
        );
        // 4:2:0 interpolates down the column too: the two rows of a 1×4 frame differ.
        let f = frame(
            (1, 4),
            8,
            ChromaSubsampling::Cs420,
            [vec![128; 4], vec![16, 240], vec![128, 128]],
        );
        let interpolated = to_rgba8(&f, colour, ChromaUpsampling::Interpolated).unwrap();
        let nearest = to_rgba8(&f, colour, ChromaUpsampling::Nearest).unwrap();
        assert_eq!(nearest[0..4], nearest[4..8]);
        assert_ne!(interpolated[4..8], nearest[4..8]);
    }

    #[test]
    fn alpha_takes_the_auxiliarys_luma_at_its_own_depth() {
        let mut rgba = vec![0u8; 8];
        let alpha = frame(
            (2, 1),
            10,
            ChromaSubsampling::Cs400,
            [vec![0, 1023], vec![], vec![]],
        );
        apply_alpha(&mut rgba, (2, 1), &alpha, AlphaChroma::Monochrome, ORIGIN).unwrap();
        assert_eq!(rgba, [0, 0, 0, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn a_coloured_alpha_is_refused_only_under_the_monochrome_policy() {
        let alpha = frame(
            (2, 1),
            8,
            ChromaSubsampling::Cs444,
            [vec![7, 9], vec![1, 1], vec![2, 2]],
        );
        let mut rgba = vec![0u8; 8];
        assert_eq!(
            refusal(apply_alpha(
                &mut rgba,
                (2, 1),
                &alpha,
                AlphaChroma::Monochrome,
                ORIGIN
            )),
            (
                ErrorKind::InvalidInput,
                Some("HEIF: alpha auxiliary must be monochrome")
            )
        );
        apply_alpha(&mut rgba, (2, 1), &alpha, AlphaChroma::Any, ORIGIN).unwrap();
        assert_eq!((rgba[3], rgba[7]), (7, 9));
        let mismatch = Some("HEIF: alpha auxiliary dimensions do not match the master image");
        for size in [(1, 1), (2, 2)] {
            assert_eq!(
                refusal(apply_alpha(
                    &mut rgba,
                    size,
                    &alpha,
                    AlphaChroma::Any,
                    ORIGIN
                ))
                .1,
                mismatch
            );
        }
    }

    /// A `w × h` buffer whose pixel `i` is `[i, i, i, i]`.
    fn numbered(w: u32, h: u32) -> Vec<u8> {
        (0..(w * h) as u8).flat_map(|i| [i; 4]).collect()
    }

    /// The pixel numbers of an RGBA buffer built by [`numbered`].
    fn order(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks(4).map(|px| px[0]).collect()
    }

    #[test]
    fn rotation_and_mirrors_move_pixels_as_specified() {
        // 3×2:  0 1 2 / 3 4 5.
        let src = numbered(3, 2);
        let (rotated, w, h) = rotate90_ccw(&src, 3, 2, ORIGIN).unwrap();
        // Anti-clockwise: the right column becomes the top row.
        assert_eq!((w, h), (2, 3));
        assert_eq!(order(&rotated), [2, 5, 1, 4, 0, 3]);
        assert_eq!(
            order(&mirror(&src, 3, 2, 1, ORIGIN).unwrap()),
            [2, 1, 0, 5, 4, 3]
        );
        assert_eq!(
            order(&mirror(&src, 3, 2, 0, ORIGIN).unwrap()),
            [3, 4, 5, 0, 1, 2]
        );
    }

    fn clap(
        width: (u32, u32),
        height: (u32, u32),
        h_off: (i32, u32),
        v_off: (i32, u32),
    ) -> CleanAperture {
        CleanAperture {
            width_n: width.0,
            width_d: width.1,
            height_n: height.0,
            height_d: height.1,
            horiz_off_n: h_off.0 as u32,
            horiz_off_d: h_off.1,
            vert_off_n: v_off.0 as u32,
            vert_off_d: v_off.1,
        }
    }

    #[test]
    fn clap_crops_the_centred_window_moved_by_its_signed_offset() {
        // 4×4: rows 0..3, 4..7, 8..11, 12..15.
        let src = numbered(4, 4);
        let (centred, w, h) =
            crop_clap(&src, 4, 4, &clap((2, 1), (2, 1), (0, 1), (0, 1)), ORIGIN).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(order(&centred), [5, 6, 9, 10]);
        // Offset −1 horizontally (two's complement), +1 vertically; halves combine exactly.
        let (moved, _, _) =
            crop_clap(&src, 4, 4, &clap((2, 1), (2, 1), (-2, 2), (1, 1)), ORIGIN).unwrap();
        assert_eq!(order(&moved), [8, 9, 12, 13]);
    }

    #[test]
    fn a_clap_that_is_not_a_whole_window_inside_the_image_is_refused() {
        let src = numbered(4, 4);
        let crop = |c| refusal(crop_clap(&src, 4, 4, &c, ORIGIN)).1;
        let zero_denominator = Some("HEIF: clap has a zero denominator");
        assert_eq!(crop(clap((2, 0), (2, 1), (0, 1), (0, 1))), zero_denominator);
        assert_eq!(crop(clap((2, 1), (2, 0), (0, 1), (0, 1))), zero_denominator);
        assert_eq!(crop(clap((2, 1), (2, 1), (0, 0), (0, 1))), zero_denominator);
        assert_eq!(crop(clap((2, 1), (2, 1), (0, 1), (0, 0))), zero_denominator);
        let fractional = Some("HEIF: clap width/height is not integer-valued");
        assert_eq!(crop(clap((3, 2), (2, 1), (0, 1), (0, 1))), fractional);
        assert_eq!(crop(clap((2, 1), (3, 2), (0, 1), (0, 1))), fractional);
        let empty = Some("HEIF: clap has a zero-sized crop");
        assert_eq!(crop(clap((0, 1), (2, 1), (0, 1), (0, 1))), empty);
        assert_eq!(crop(clap((2, 1), (0, 1), (0, 1), (0, 1))), empty);
        // A 3-wide window in 4 centres on a half pixel.
        let off_grid = Some("HEIF: clap offset is not integer-valued");
        assert_eq!(crop(clap((3, 1), (2, 1), (0, 1), (0, 1))), off_grid);
        assert_eq!(crop(clap((2, 1), (3, 1), (0, 1), (0, 1))), off_grid);
        let outside = Some("HEIF: clap crop window lies outside the image");
        assert_eq!(crop(clap((2, 1), (2, 1), (-2, 1), (0, 1))), outside);
        assert_eq!(crop(clap((2, 1), (2, 1), (0, 1), (-2, 1))), outside);
        assert_eq!(crop(clap((2, 1), (2, 1), (2, 1), (0, 1))), outside);
        assert_eq!(crop(clap((2, 1), (2, 1), (0, 1), (2, 1))), outside);
        // The whole image is a window inside it.
        assert!(crop_clap(&src, 4, 4, &clap((4, 1), (4, 1), (0, 1), (0, 1)), ORIGIN).is_ok());
    }

    #[test]
    fn transforms_apply_in_list_order() {
        let src = (numbered(3, 2), 3, 2);
        let rotate_then_mirror = apply_transforms(
            vec![
                TransformativeProperty::Rotation(1),
                TransformativeProperty::Mirror(1),
            ],
            src.clone(),
            ORIGIN,
        )
        .unwrap();
        let mirror_then_rotate = apply_transforms(
            vec![
                TransformativeProperty::Mirror(1),
                TransformativeProperty::Rotation(1),
            ],
            src,
            ORIGIN,
        )
        .unwrap();
        assert_eq!(order(&rotate_then_mirror.0), [5, 2, 4, 1, 3, 0]);
        assert_eq!(order(&mirror_then_rotate.0), [0, 3, 1, 4, 2, 5]);
        assert_eq!((rotate_then_mirror.1, rotate_then_mirror.2), (2, 3));
    }

    #[test]
    fn source_over_blends_unassociated_alpha_and_clips_to_the_canvas() {
        // An opaque red canvas under a half-transparent green source.
        let mut canvas = [255u8, 0, 0, 255].repeat(2);
        composite_over(&mut canvas, (2, 1), &[0, 255, 0, 128], (1, 1), (1, 0));
        // out_a = 128 + round(255·127/255) = 255; red = round(round(255·255·127/255)/255) = 127.
        assert_eq!(canvas, [255, 0, 0, 255, 127, 128, 0, 255]);
        // Two transparent pixels make a transparent pixel, whatever their colour.
        let mut clear = [9u8, 9, 9, 0];
        composite_over(&mut clear, (1, 1), &[7, 7, 7, 0], (1, 1), (0, 0));
        assert_eq!(clear, [0; 4]);
        // A source hanging off every edge writes only the overlap.
        let mut canvas = [0u8; 4];
        let src = [200u8, 200, 200, 255].repeat(9);
        composite_over(&mut canvas, (1, 1), &src, (3, 3), (-1, -1));
        assert_eq!(canvas, [200, 200, 200, 255]);
        let mut untouched = [1u8, 2, 3, 4];
        composite_over(&mut untouched, (1, 1), &src[..4], (1, 1), (1, 0));
        composite_over(&mut untouched, (1, 1), &src[..4], (1, 1), (0, 1));
        composite_over(&mut untouched, (1, 1), &src[..4], (1, 1), (-1, 0));
        composite_over(&mut untouched, (1, 1), &src[..4], (1, 1), (0, -1));
        assert_eq!(untouched, [1, 2, 3, 4]);
    }
}
