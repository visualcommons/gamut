//! Sharp-YUV: libwebp's iterative RGB → YCbCr 4:2:0 conversion (`sharpyuv/sharpyuv.c`, libwebp
//! 1.5.0), ported for 8-bit RGB in and 8-bit YCbCr out with the sRGB transfer function — the
//! configuration `cwebp -sharp_yuv` runs (`WebPPictureSharpARGBToYUVA`).
//!
//! A box filter averages chroma in gamma space and leaves luma alone, so a sharp colour edge
//! inside a 2×2 block decodes with a halo: the shared chroma sample is wrong for both sides, and
//! nothing compensates in luma. Sharp-YUV instead treats the 4:2:0 planes as the unknowns of a
//! reconstruction problem. It works on a "W/RGB" split — a per-pixel grey `W` plus a per-block
//! `RGB − W` chroma residual, 2 bits above the 8-bit input — and for four rounds it upsamples the
//! current estimate the way a fancy-upsampling decoder will (a 9-3-3-1 filter), measures how far
//! the result's linear-light luma and 2×2 linear-light chroma are from the source's, and folds the
//! error back into the estimate. It stops early once the luma error is under 3 per pixel or stops
//! shrinking. Only then is the estimate matrixed to YCbCr.
//!
//! Every step is integer arithmetic except the two gamma tables, which libwebp builds once from
//! `pow` at start-up; they are built here the same way. The C reference's SIMD kernels are
//! bit-identical to its scalar ones, which this ports.

use std::sync::LazyLock;

/// Bits of extra precision the W/RGB working space carries above 8-bit input (`GetPrecisionShift`
/// for `rgb_bit_depth = 8`).
const SFIX: u32 = 2;
/// The W/RGB working space's bit depth: 8-bit input plus [`SFIX`].
const WORK_BITS: u32 = 8 + SFIX;
/// The largest value of the working space.
const WORK_MAX: i32 = (1 << WORK_BITS) - 1;
/// Fixed-point fraction bits of the RGB → YCbCr matrix rows.
const YUV_FIX: u32 = 16;
/// Refinement rounds (`kNumIterations`).
const ITERATIONS: usize = 4;

/// Gamma → linear table resolution in bits (`GAMMA_TO_LINEAR_TAB_BITS`), equal to [`WORK_BITS`],
/// which is why a lookup needs no interpolation.
const G2L_BITS: u32 = 10;
/// Linear → gamma table resolution in bits (`LINEAR_TO_GAMMA_TAB_BITS`).
const L2G_BITS: u32 = 9;
/// Fixed-point bits of a linear value (`GAMMA_TO_LINEAR_BITS`).
const LINEAR_BITS: u32 = 16;

/// The two sRGB-curve tables libwebp's `SharpYuvInitGammaTables` builds: gamma → linear (1026
/// entries, the last duplicated as an overrun guard) and linear → gamma (514 entries, likewise).
struct GammaTables {
    to_linear: [u32; (1 << G2L_BITS) + 2],
    to_gamma: [u32; (1 << L2G_BITS) + 2],
}

/// The tables, built on first use.
static TABLES: LazyLock<GammaTables> = LazyLock::new(build_tables);

/// Builds [`GammaTables`] exactly as `SharpYuvInitGammaTables` does — the BT.709-shaped curve
/// (`a = 0.0993`, exponent `1/0.45`) libwebp uses for its "sRGB" transfer.
fn build_tables() -> GammaTables {
    const A: f64 = 0.099_296_826_809_44;
    const THRESH: f64 = 0.018_053_968_510_807;
    const GAMMA: f64 = 1.0 / 0.45;
    let scale = f64::from(1u32 << LINEAR_BITS);
    let mut to_linear = [0u32; (1 << G2L_BITS) + 2];
    let norm = 1.0 / f64::from(1u32 << G2L_BITS);
    for (v, slot) in to_linear.iter_mut().take((1 << G2L_BITS) + 1).enumerate() {
        let g = norm * v as f64;
        let value = if g <= THRESH * 4.5 {
            g / 4.5
        } else {
            ((g + A) / (1.0 + A)).powf(GAMMA)
        };
        *slot = (value * scale + 0.5) as u32;
    }
    to_linear[(1 << G2L_BITS) + 1] = to_linear[1 << G2L_BITS];
    let mut to_gamma = [0u32; (1 << L2G_BITS) + 2];
    let norm = 1.0 / f64::from(1u32 << L2G_BITS);
    for (v, slot) in to_gamma.iter_mut().take((1 << L2G_BITS) + 1).enumerate() {
        let g = norm * v as f64;
        let value = if g <= THRESH {
            4.5 * g
        } else {
            (1.0 + A) * g.powf(1.0 / GAMMA) - A
        };
        *slot = (scale * value + 0.5) as u32;
    }
    to_gamma[(1 << L2G_BITS) + 1] = to_gamma[1 << L2G_BITS];
    GammaTables {
        to_linear,
        to_gamma,
    }
}

impl GammaTables {
    /// `ToLinearSrgb` at the working depth: the table has one entry per working-space value.
    fn linear(&self, v: u16) -> u32 {
        self.to_linear[usize::from(v)]
    }

    /// `FromLinearSrgb` at the working depth: linear interpolation between the two table entries
    /// bracketing `value`, each narrowed from 16 to [`WORK_BITS`] bits first.
    fn gamma(&self, value: u32) -> u16 {
        const POS_SHIFT: u32 = LINEAR_BITS - L2G_BITS;
        let pos = (value >> POS_SHIFT) as usize;
        let frac = value - ((pos as u32) << POS_SHIFT);
        let v0 = self.to_gamma[pos] >> (LINEAR_BITS - WORK_BITS);
        let v1 = self.to_gamma[pos + 1] >> (LINEAR_BITS - WORK_BITS);
        (v0 + (((v1 - v0) * frac + (1 << (POS_SHIFT - 1))) >> POS_SHIFT)) as u16
    }
}

/// `RGBToGray`: the luma weights sharp-YUV measures `W` with, in Q16.
fn gray(r: i64, g: i64, b: i64) -> i32 {
    ((13933 * r + 46871 * g + 4732 * b + (1 << (YUV_FIX - 1))) >> YUV_FIX) as i32
}

/// Clamps to the working space.
fn clip_work(v: i32) -> u16 {
    v.clamp(0, WORK_MAX) as u16
}

/// One RGB → YCbCr row in Q16 plus its offset, as libwebp's `SharpYuvConversionMatrix` holds it.
pub(crate) type MatrixRow = [i32; 4];

/// The working-space planes one pass reads and writes. Rows are `w` wide (the even-rounded width)
/// for the `W` planes and `3 * uv_w` wide (R, G and B residual rows back to back) for chroma.
struct Work {
    w: usize,
    uv_w: usize,
}

impl Work {
    /// `UpdateW`: the linear-light luma of each pixel of a three-plane row, back in gamma space.
    fn update_w(&self, t: &GammaTables, src: &[u16], dst: &mut [u16]) {
        let w = self.w;
        for i in 0..w {
            let r = t.linear(src[i]);
            let g = t.linear(src[w + i]);
            let b = t.linear(src[2 * w + i]);
            dst[i] = t.gamma(gray(r.into(), g.into(), b.into()) as u32);
        }
    }

    /// `UpdateChroma`: per 2×2 block, the gamma-space value of each channel's linear-light mean,
    /// stored as its residual from that mean's grey.
    fn update_chroma(&self, t: &GammaTables, src1: &[u16], src2: &[u16], dst: &mut [i16]) {
        let (w, uv_w) = (self.w, self.uv_w);
        let down = |plane: usize, i: usize| {
            let o = plane * w + 2 * i;
            let sum = t.linear(src1[o])
                + t.linear(src1[o + 1])
                + t.linear(src2[o])
                + t.linear(src2[o + 1]);
            i32::from(t.gamma((sum + 2) >> 2))
        };
        for i in 0..uv_w {
            let (r, g, b) = (down(0, i), down(1, i), down(2, i));
            let grey = gray(r.into(), g.into(), b.into());
            dst[i] = (r - grey) as i16;
            dst[uv_w + i] = (g - grey) as i16;
            dst[2 * uv_w + i] = (b - grey) as i16;
        }
    }

    /// `InterpolateTwoRows`: upsamples the chroma residual rows around one row pair with the
    /// 9-3-3-1 filter (3-1 at the left and right edges) and adds it to that pair's `W`, giving
    /// the RGB the current estimate decodes to.
    fn interpolate(
        &self,
        best_y: &[u16],
        uv: &[i16],
        rows: [usize; 3],
        out1: &mut [u16],
        out2: &mut [u16],
    ) {
        let (w, uv_w) = (self.w, self.uv_w);
        let [prev, cur, next] = rows.map(|r| r * 3 * uv_w);
        for plane in 0..3 {
            let base = plane * uv_w;
            let a = &uv[cur + base..cur + base + uv_w];
            for (out, other, y) in [
                (
                    &mut *out1,
                    &uv[prev + base..prev + base + uv_w],
                    &best_y[..w],
                ),
                (
                    &mut *out2,
                    &uv[next + base..next + base + uv_w],
                    &best_y[w..2 * w],
                ),
            ] {
                let out = &mut out[plane * w..(plane + 1) * w];
                // The 3-1 filter at a row end: chroma sample `k`, luma column `x`.
                let edge = |k: usize, x: usize| {
                    let v0 = (i32::from(a[k]) * 3 + i32::from(other[k]) + 2) >> 2;
                    clip_work(v0 + i32::from(y[x]))
                };
                out[0] = edge(0, 0);
                for i in 0..uv_w - 1 {
                    let (a0, a1) = (i32::from(a[i]), i32::from(a[i + 1]));
                    let (b0, b1) = (i32::from(other[i]), i32::from(other[i + 1]));
                    let v0 = (a0 * 9 + a1 * 3 + b0 * 3 + b1 + 8) >> 4;
                    let v1 = (a1 * 9 + a0 * 3 + b1 * 3 + b0 + 8) >> 4;
                    out[2 * i + 1] = clip_work(i32::from(y[2 * i + 1]) + v0);
                    out[2 * i + 2] = clip_work(i32::from(y[2 * i + 2]) + v1);
                }
                out[w - 1] = edge(uv_w - 1, w - 1);
            }
        }
    }
}

/// Reads one source row into the three working-space planes of `dst` (`3 * w`), scaled up by
/// [`SFIX`] bits, replicating the last pixel into the padding column of an odd width.
fn import_row(row: &[u8], width: usize, w: usize, dst: &mut [u16]) {
    for (i, px) in row.as_chunks::<3>().0.iter().take(width).enumerate() {
        for c in 0..3 {
            dst[c * w + i] = u16::from(px[c]) << SFIX;
        }
    }
    if width < w {
        for c in 0..3 {
            dst[c * w + width] = dst[c * w + width - 1];
        }
    }
}

/// `RGBToYUVComponent` on a working-space triple: the matrix row in Q16, the offset scaled into the
/// working space, and the [`SFIX`] extra bits dropped with rounding.
fn component(r: i32, g: i32, b: i32, row: MatrixRow) -> u8 {
    let round = 1 << (YUV_FIX + SFIX - 1);
    let v = (row[0] * r + row[1] * g + row[2] * b + (row[3] << SFIX) + round) >> (YUV_FIX + SFIX);
    v.clamp(0, 255) as u8
}

/// Converts interleaved 8-bit RGB (`width * height * 3` bytes, rows packed) to `Y/Cb/Cr` 4:2:0
/// planes through sharp-YUV, matrixing with the `[y, u, v]` rows given (Q16 coefficients and the
/// unscaled 8-bit offset, `16 << 16` for limited-range luma).
///
/// The chroma planes are `ceil(width / 2) × ceil(height / 2)`.
pub(crate) fn rgb8_to_ycbcr420(
    px: &[u8],
    width: usize,
    height: usize,
    matrix: [MatrixRow; 3],
) -> [Vec<u8>; 3] {
    let t = &*TABLES;
    let w = (width + 1) & !1;
    let h = (height + 1) & !1;
    let uv_w = w / 2;
    let uv_h = h / 2;
    let work = Work { w, uv_w };
    let uv_row = 3 * uv_w;

    let mut best_y = vec![0u16; w * h];
    let mut target_y = vec![0u16; w * h];
    let mut best_uv = vec![0i16; uv_row * uv_h];
    let mut target_uv = vec![0i16; uv_row * uv_h];
    let mut src1 = vec![0u16; 3 * w];
    let mut src2 = vec![0u16; 3 * w];

    // Import: the source's own W (its gamma-space grey) seeds the estimate; the linear-light
    // targets are what every round is measured against.
    for j in (0..height).step_by(2) {
        import_row(&px[j * width * 3..(j + 1) * width * 3], width, w, &mut src1);
        if j + 1 < height {
            import_row(
                &px[(j + 1) * width * 3..(j + 2) * width * 3],
                width,
                w,
                &mut src2,
            );
        } else {
            src2.copy_from_slice(&src1);
        }
        for (k, src) in [&src1, &src2].into_iter().enumerate() {
            let row = &mut best_y[(j + k) * w..(j + k + 1) * w];
            for i in 0..w {
                row[i] = gray(src[i].into(), src[w + i].into(), src[2 * w + i].into()) as u16;
            }
        }
        work.update_w(t, &src1, &mut target_y[j * w..(j + 1) * w]);
        work.update_w(t, &src2, &mut target_y[(j + 1) * w..(j + 2) * w]);
        let r = j / 2;
        work.update_chroma(
            t,
            &src1,
            &src2,
            &mut target_uv[r * uv_row..(r + 1) * uv_row],
        );
    }
    best_uv.copy_from_slice(&target_uv);

    // Refine. Rows are updated in place, top to bottom, so each pair interpolates against the
    // already-updated row above and the not-yet-updated row below — the reference's order.
    let threshold = 3 * (w * h) as u64;
    let mut prev_diff = u64::MAX;
    let mut rgb_y = vec![0u16; 2 * w];
    let mut rgb_uv = vec![0i16; uv_row];
    for iter in 0..ITERATIONS {
        let mut diff = 0u64;
        let mut prev = 0;
        for r in 0..uv_h {
            let next = if r + 1 < uv_h { r + 1 } else { r };
            let pair = 2 * r * w..2 * (r + 1) * w;
            work.interpolate(
                &best_y[pair.clone()],
                &best_uv,
                [prev, r, next],
                &mut src1,
                &mut src2,
            );
            prev = r;
            work.update_w(t, &src1, &mut rgb_y[..w]);
            work.update_w(t, &src2, &mut rgb_y[w..]);
            work.update_chroma(t, &src1, &src2, &mut rgb_uv);
            for ((dst, &target), &got) in best_y[pair.clone()]
                .iter_mut()
                .zip(&target_y[pair])
                .zip(&rgb_y)
            {
                let delta = i32::from(target) - i32::from(got);
                *dst = clip_work(i32::from(*dst) + delta);
                diff += u64::from(delta.unsigned_abs());
            }
            let uv = r * uv_row..(r + 1) * uv_row;
            for ((dst, &target), &got) in best_uv[uv.clone()]
                .iter_mut()
                .zip(&target_uv[uv])
                .zip(&rgb_uv)
            {
                *dst = dst.wrapping_add(target.wrapping_sub(got));
            }
        }
        if iter > 0 && (diff < threshold || diff > prev_diff) {
            break;
        }
        prev_diff = diff;
    }

    // Matrix the estimate. Chroma rows carry `RGB − W`; a constant offset on all three channels
    // leaves Cb and Cr unchanged, so they are matrixed without adding `W` back.
    let [my, mu, mv] = matrix;
    let mut y = vec![0u8; width * height];
    for j in 0..height {
        let uv = &best_uv[(j / 2) * uv_row..(j / 2 + 1) * uv_row];
        for i in 0..width {
            let grey = i32::from(best_y[j * w + i]);
            let o = i / 2;
            let (r, g, b) = (
                i32::from(uv[o]) + grey,
                i32::from(uv[uv_w + o]) + grey,
                i32::from(uv[2 * uv_w + o]) + grey,
            );
            y[j * width + i] = component(r, g, b, my);
        }
    }
    let mut u = vec![0u8; uv_w * uv_h];
    let mut v = vec![0u8; uv_w * uv_h];
    for r in 0..uv_h {
        let uv = &best_uv[r * uv_row..(r + 1) * uv_row];
        for i in 0..uv_w {
            let (cr, cg, cb) = (
                i32::from(uv[i]),
                i32::from(uv[uv_w + i]),
                i32::from(uv[2 * uv_w + i]),
            );
            u[r * uv_w + i] = component(cr, cg, cb, mu);
            v[r * uv_w + i] = component(cr, cg, cb, mv);
        }
    }
    [y, u, v]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cicp::ColorRange;
    use crate::ycbcr::bt601_rows;

    /// FNV-1a (64-bit), to pin whole planes in one literal.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// A `w × h` image from a per-pixel generator.
    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|i| f(i % w, i / w)).collect()
    }

    fn digests(px: &[u8], w: usize, h: usize, range: ColorRange) -> [u64; 3] {
        rgb8_to_ycbcr420(px, w, h, bt601_rows(range)).map(|p| fnv1a64(&p))
    }

    /// The two `pow`-built tables, pinned by sum and by the entries the curve's ends decide. The
    /// tables are the only floating-point step, so this is where a platform `pow` that rounds
    /// differently from glibc's would surface first.
    #[test]
    fn gamma_tables_match_libwebps_construction() {
        let t = &*TABLES;
        let sum = |tab: &[u32]| tab.iter().map(|&v| u64::from(v)).sum::<u64>();
        assert_eq!(sum(&t.to_linear), 22_974_572);
        assert_eq!(sum(&t.to_gamma), 22_214_576);
        assert_eq!(
            [t.to_linear[0], t.to_linear[1], t.to_linear[1025]],
            [0, 14, 65536]
        );
        assert_eq!(
            [t.to_gamma[0], t.to_gamma[1], t.to_gamma[513]],
            [0, 576, 65536]
        );
    }

    /// Whole-plane pins, captured from this port, which `gamut-webp`'s oracle suite
    /// (`gamut_sharp_yuv_matches_libwebp_bit_exactly`) holds byte-equal to libwebp's
    /// `WebPPictureSharpARGBToYUVA` for limited range. `edges` alternates saturated magenta and
    /// green on a diagonal at odd dimensions, so the working space hits both clamps and the
    /// padding column and duplicated last row are read; `smooth` is a gradient the refinement
    /// settles on early. Full range shares every step but the matrix rows.
    #[test]
    fn planes_are_pinned_in_both_ranges() {
        let edges = image(7, 5, |x, y| {
            if (x + y) % 3 == 0 {
                [255, 0, 255]
            } else {
                [0, 255, 0]
            }
        });
        let smooth = image(16, 6, |x, y| [(x * 8) as u8, (y * 20) as u8, 100]);
        assert_eq!(
            digests(&edges, 7, 5, ColorRange::Limited),
            [
                9_754_219_132_552_498_603,
                2_582_034_964_411_161_735,
                10_380_516_979_300_429_402
            ]
        );
        assert_eq!(
            digests(&edges, 7, 5, ColorRange::Full),
            [
                6_961_482_050_530_981_237,
                7_937_959_219_933_158_466,
                7_252_107_136_850_292_776
            ]
        );
        assert_eq!(
            digests(&smooth, 16, 6, ColorRange::Limited),
            [
                15_665_729_297_358_490_980,
                3_829_192_075_110_364_339,
                7_839_833_363_473_970_716
            ]
        );
        assert_eq!(
            digests(&smooth, 16, 6, ColorRange::Full),
            [
                4_576_387_768_385_306_237,
                2_538_998_086_756_588_465,
                11_279_614_931_882_225_096
            ]
        );
    }

    /// A two-pixel-wide image has a single chroma column, so both row ends of the interpolation
    /// are the same chroma sample and must still read their own luma column: red and blue bars
    /// keep distinct luma rather than one copying the other.
    #[test]
    fn a_single_chroma_column_keeps_both_luma_columns() {
        let bars = image(
            2,
            3,
            |x, _| {
                if x == 0 { [250, 10, 10] } else { [10, 10, 250] }
            },
        );
        let [y, u, v] = rgb8_to_ycbcr420(&bars, 2, 3, bt601_rows(ColorRange::Limited));
        assert_eq!(y, [105, 38, 105, 38, 105, 38]);
        assert_eq!((u, v), (vec![182, 182], vec![197, 197]));
    }
}
