//! Macroblock analysis and spatial noise shaping (SNS): libwebp 1.5.0's segment assignment
//! (`src/enc/analysis_enc.c`) and per-segment quantizers (`VP8SetSegmentParams`,
//! `src/enc/quant_enc.c`), ported so the same planes and quality land on the same quantizer per
//! macroblock.
//!
//! Each macroblock gets a *susceptibility* `alpha`: how far the DCT spectrum of its best
//! source-predicted residual reaches, relative to its peak. A flat block's energy sits in the
//! lowest bins (low alpha, "easy"); a textured one spreads it (high alpha). A six-round k-means
//! over the frame's alpha histogram clusters the macroblocks into four segments, and SNS then
//! moves each segment's quantizer away from the quality's base along `c^(1 − amp·alpha)`:
//! textured segments, where quantization noise is masked, are quantized harder, flat ones
//! more gently. The chroma AC and DC quantizers shift by frame-wide deltas derived the same way.
//!
//! The analysis predicts from the **source** (libwebp's "pretend we have perfect lossless
//! reconstruction"), so it depends on nothing the encoder decides later, and it reads the
//! edge-replicated padded planes, which equal libwebp's per-macroblock import.

use super::transform::fdct4x4;

/// The largest susceptibility (`MAX_ALPHA` in `analysis_enc.c`).
const MAX_ALPHA: i32 = 255;
/// Histogram bins a coefficient magnitude is clipped into (`MAX_COEFF_THRESH`).
const MAX_COEFF_THRESH: usize = 31;
/// k-means rounds (`MAX_ITERS_K_MEANS`).
const MAX_ITERS_K_MEANS: usize = 6;
/// Segments libwebp codes by default (`WebPConfig::segments`).
const NUM_SEGMENTS: usize = 4;

/// Scales SNS strength into a quantizer exponent (`SNS_TO_DQ`).
const SNS_TO_DQ: f64 = 0.9;
/// Neutral chroma susceptibility (`MID_ALPHA` in `quant_enc.c`).
const MID_UV_ALPHA: i32 = 64;
/// Lowest useful chroma susceptibility (`MIN_ALPHA`).
const MIN_UV_ALPHA: i32 = 30;
/// Highest useful chroma susceptibility (`MAX_ALPHA` in `quant_enc.c`).
const MAX_UV_ALPHA: i32 = 100;
/// Largest chroma AC quantizer delta SNS applies (`MAX_DQ_UV`).
const MAX_DQ_UV: i32 = 6;
/// Smallest chroma AC quantizer delta SNS applies (`MIN_DQ_UV`).
const MIN_DQ_UV: i32 = -4;

/// The quantizer plan SNS settles on: which segment each macroblock is in, each segment's base
/// quantizer index, and the frame's chroma deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SegmentPlan {
    /// The segment of each macroblock, in raster order.
    pub map: Vec<usize>,
    /// Each segment's quantizer index; entries past `count` repeat the last used one.
    pub quant: [u8; NUM_SEGMENTS],
    /// Distinct segments after equal quantizers are merged; `1` means segmentation is off.
    pub count: usize,
    /// Chroma DC quantizer delta (`dq_uv_dc`).
    pub uv_dc_delta: i8,
    /// Chroma AC quantizer delta (`dq_uv_ac`).
    pub uv_ac_delta: i8,
}

/// libwebp's `QualityToCompression` (`src/enc/quant_enc.c`): the compressibility `c_base` a
/// `0..=100` quality (saturating) asks for. With `c = quality / 100`, the piecewise-linear
/// `linear_c = c < 0.75 ? c·2/3 : 2c − 1` is raised to `1/3` (file size scales roughly as the cube
/// of the quantizer).
///
/// The two lines meet at `c = 0.75` (both `0.5`), and below it `c·2/3` is the larger of the two, so
/// `linear_c` is written as their `max` rather than as libwebp's branch: the branch's `<` and `<=`
/// are indistinguishable on every input, and `max` leaves no such twin.
fn quality_to_compression(quality: u8) -> f64 {
    let c = f64::from(quality.min(100)) / 100.0;
    let linear_c = (c * (2.0 / 3.0)).max(2.0 * c - 1.0);
    linear_c.powf(1.0 / 3.0)
}

/// The quantizer index (`0..=127`) SNS gives a segment of susceptibility `alpha` at compressibility
/// `c_base` and modulation `amp`: `⌊127·(1 − c_base^(1 − amp·alpha))⌋`. At `amp = 0` it is the
/// quality's unmodulated base quantizer — libwebp's at `sns_strength = 0`, which the encoder's
/// oracle test checks at every quality. Over `0..=100` that floor's fractional distance from an
/// integer is at least `0.0119`, so a last-ulp difference in a platform's `powf` cannot move it.
fn segment_quant(c_base: f64, amp: f64, alpha: i32) -> u8 {
    let c = c_base.powf(1.0 - amp * f64::from(alpha));
    // `c` lies in `0.0..=1.0` (a base in that range to a positive power), so the product is in
    // `0.0..=127.0` and the truncating cast is libwebp's `(int)` floor and its clip both.
    (127.0 * (1.0 - c)) as u8
}

/// The padded source planes the analysis reads, with their strides, and the macroblock grid.
pub(super) struct Source<'a> {
    /// The luma plane, padded to whole macroblocks.
    pub y: &'a [u8],
    /// The Cb plane, padded to whole macroblocks.
    pub u: &'a [u8],
    /// The Cr plane, padded to whole macroblocks.
    pub v: &'a [u8],
    /// Samples per luma row.
    pub y_stride: usize,
    /// Samples per chroma row.
    pub c_stride: usize,
    /// Macroblock columns.
    pub mb_cols: usize,
    /// Macroblock rows.
    pub mb_rows: usize,
}

/// The two predictors the analysis tries, in libwebp's order (`VP8I16ModeOffsets[0..2]`).
#[derive(Clone, Copy)]
enum Mode {
    Dc,
    TrueMotion,
}

/// One `size`-square source-edge prediction (`DCMode` / `TrueMotion` in `src/dsp/enc.c`, with
/// libwebp's encoder-side fallbacks for a missing edge rather than the decoder's 127/129 borders).
fn predict(
    mode: Mode,
    left: Option<&[u8]>,
    top: Option<&[u8]>,
    corner: u8,
    size: usize,
) -> Vec<u8> {
    let fill = |v: u8| vec![v; size * size];
    match mode {
        Mode::Dc => {
            let sum = |edge: &[u8]| edge.iter().map(|&p| u32::from(p)).sum::<u32>();
            let shift = size.trailing_zeros() + 1;
            let dc = match (top, left) {
                (Some(t), Some(l)) => (sum(t) + sum(l) + size as u32) >> shift,
                (Some(e), None) | (None, Some(e)) => (2 * sum(e) + size as u32) >> shift,
                (None, None) => 0x80,
            };
            fill(dc as u8)
        }
        Mode::TrueMotion => match (left, top) {
            (Some(l), Some(t)) => (0..size * size)
                .map(|i| {
                    let v = i32::from(t[i % size]) + i32::from(l[i / size]) - i32::from(corner);
                    v.clamp(0, 255) as u8
                })
                .collect(),
            (Some(l), None) => (0..size * size).map(|i| l[i / size]).collect(),
            (None, Some(t)) => (0..size * size).map(|i| t[i % size]).collect(),
            (None, None) => fill(129),
        },
    }
}

/// Adds the coefficient-magnitude bins of every 4×4 block of the `size`-square block at `(x, y)`
/// of `plane` against `pred` into `bins` (`CollectHistogram`).
fn collect(
    bins: &mut [u32; MAX_COEFF_THRESH + 1],
    plane: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    pred: &[u8],
    size: usize,
) {
    for by in (0..size).step_by(4) {
        for bx in (0..size).step_by(4) {
            let residue: [i16; 16] = core::array::from_fn(|k| {
                let (r, c) = (by + k / 4, bx + k % 4);
                i16::from(plane[(y + r) * stride + x + c]) - i16::from(pred[r * size + c])
            });
            for coeff in fdct4x4(&residue) {
                let bin = usize::from(coeff.unsigned_abs() >> 3).min(MAX_COEFF_THRESH);
                bins[bin] += 1;
            }
        }
    }
}

/// `GetAlpha` over the histogram `VP8SetHistogramData` reduces `bins` to: the last occupied bin
/// scaled by the tallest one.
///
/// libwebp returns `0` unless the tallest bin holds more than one coefficient. That guard is
/// omitted because no histogram here can trip it: the fewest coefficients one holds is chroma's
/// 128 (eight 4×4 blocks), and 128 values in 32 bins put at least 4 in one.
fn alpha(bins: &[u32; MAX_COEFF_THRESH + 1]) -> i32 {
    let max_value = bins.iter().copied().max().unwrap_or(1) as i32;
    let last_non_zero = bins.iter().rposition(|&n| n > 0).unwrap_or(1) as i32;
    2 * MAX_ALPHA * last_non_zero / max_value
}

/// The top, left and corner source edges of the `size`-square block at `(x, y)`, or `None` on the
/// frame's top row / left column.
fn edges(
    plane: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    size: usize,
) -> (Option<Vec<u8>>, Option<Vec<u8>>, u8) {
    let top = (y > 0).then(|| plane[(y - 1) * stride + x..(y - 1) * stride + x + size].to_vec());
    let left = (x > 0).then(|| (0..size).map(|r| plane[(y + r) * stride + x - 1]).collect());
    let corner = if x > 0 && y > 0 {
        plane[(y - 1) * stride + x - 1]
    } else {
        0
    };
    (top, left, corner)
}

/// The best (highest) alpha of the two predictors over the `size`-square blocks at `(x, y)` of
/// each plane in `planes` (one plane for luma, U and V together for chroma).
fn best_alpha(planes: &[&[u8]], stride: usize, x: usize, y: usize, size: usize) -> i32 {
    let edges: Vec<_> = planes
        .iter()
        .map(|p| edges(p, stride, x, y, size))
        .collect();
    let mut best = -1;
    for mode in [Mode::Dc, Mode::TrueMotion] {
        let mut bins = [0u32; MAX_COEFF_THRESH + 1];
        for (plane, (top, left, corner)) in planes.iter().zip(&edges) {
            let pred = predict(mode, left.as_deref(), top.as_deref(), *corner, size);
            collect(&mut bins, plane, stride, x, y, &pred, size);
        }
        best = best.max(alpha(&bins));
    }
    best
}

/// `AssignSegments`: clusters the alpha histogram into [`NUM_SEGMENTS`] by k-means, returning
/// each alpha's cluster, the cluster centres, and their weighted mean.
fn k_means(
    histogram: &[u32; MAX_ALPHA as usize + 1],
) -> ([usize; MAX_ALPHA as usize + 1], [i32; NUM_SEGMENTS], i32) {
    let nb = NUM_SEGMENTS as i32;
    let min_a = histogram
        .iter()
        .position(|&n| n > 0)
        .unwrap_or(MAX_ALPHA as usize + 1) as i32;
    let max_a = (min_a..=MAX_ALPHA)
        .rev()
        .find(|&a| histogram[a as usize] > 0)
        .unwrap_or(min_a);
    let range_a = max_a - min_a;
    let mut centers: [i32; NUM_SEGMENTS] =
        core::array::from_fn(|k| min_a + (2 * k as i32 + 1) * range_a / (2 * nb));
    let mut map = [0usize; MAX_ALPHA as usize + 1];
    let mut weighted_average = 0;
    for _ in 0..MAX_ITERS_K_MEANS {
        let mut accum = [0i64; NUM_SEGMENTS];
        let mut dist_accum = [0i64; NUM_SEGMENTS];
        let mut n = 0;
        for a in min_a..=max_a {
            let count = i64::from(histogram[a as usize]);
            if count > 0 {
                while n + 1 < NUM_SEGMENTS && (a - centers[n + 1]).abs() < (a - centers[n]).abs() {
                    n += 1;
                }
                map[a as usize] = n;
                dist_accum[n] += i64::from(a) * count;
                accum[n] += count;
            }
        }
        let mut displaced = 0;
        let (mut weighted, mut total_weight) = (0i64, 0i64);
        for n in 0..NUM_SEGMENTS {
            if accum[n] > 0 {
                let new_center = ((dist_accum[n] + accum[n] / 2) / accum[n]) as i32;
                displaced += (centers[n] - new_center).abs();
                centers[n] = new_center;
                weighted += i64::from(new_center) * accum[n];
                total_weight += accum[n];
            }
        }
        weighted_average = ((weighted + total_weight / 2) / total_weight) as i32;
        if displaced < 5 {
            break;
        }
    }
    (map, centers, weighted_average)
}

/// `SetSegmentAlphas`: each centre's distance from `mid`, scaled to the centres' spread and
/// clipped to `-127..=127`.
fn segment_alphas(centers: &[i32; NUM_SEGMENTS], mid: i32) -> [i32; NUM_SEGMENTS] {
    let min = centers.iter().copied().min().unwrap_or(0);
    let mut max = centers.iter().copied().max().unwrap_or(0);
    if max == min {
        max = min + 1;
    }
    centers.map(|c| (255 * (c - mid) / (max - min)).clamp(-127, 127))
}

/// Runs the analysis and SNS over `src` for a frame coded at `quality` with SNS `strength`
/// (`0..=100`; `0` leaves every segment at the base quantizer, which merges them into one).
/// `fast` is libwebp's `method <= 1` path, which skips the luma predictors and scores luma as
/// perfectly predictable.
pub(super) fn plan_segments(
    src: &Source<'_>,
    quality: u8,
    strength: u8,
    fast: bool,
) -> SegmentPlan {
    // libwebp's `WebPValidateConfig` refuses a strength above 100; gamut saturates instead, as
    // it does `quality`.
    let strength = strength.min(100);
    let total = src.mb_cols * src.mb_rows;
    let mut histogram = [0u32; MAX_ALPHA as usize + 1];
    let mut mb_alpha = Vec::with_capacity(total);
    let mut uv_alpha_sum = 0i64;
    for mb_y in 0..src.mb_rows {
        for mb_x in 0..src.mb_cols {
            let luma = if fast {
                0
            } else {
                best_alpha(&[src.y], src.y_stride, mb_x * 16, mb_y * 16, 16)
            };
            let uv = best_alpha(&[src.u, src.v], src.c_stride, mb_x * 8, mb_y * 8, 8);
            let mixed = (3 * luma + uv + 2) >> 2;
            let a = (MAX_ALPHA - mixed).clamp(0, MAX_ALPHA) as usize;
            histogram[a] += 1;
            mb_alpha.push(a);
            uv_alpha_sum += i64::from(uv);
        }
    }
    let (alpha_map, centers, mid) = k_means(&histogram);
    let alphas = segment_alphas(&centers, mid);

    let amp = SNS_TO_DQ * f64::from(strength) / 100.0 / 128.0;
    let c_base = quality_to_compression(quality);
    let mut quant = alphas.map(|a| segment_quant(c_base, amp, a));

    // `SimplifySegments`: segments with equal quantizers merge into the first of them.
    let mut remap = [0usize; NUM_SEGMENTS];
    let mut count = 1;
    for s in 1..NUM_SEGMENTS {
        match (0..count).find(|&t| quant[t] == quant[s]) {
            Some(t) => remap[s] = t,
            None => {
                quant[count] = quant[s];
                remap[s] = count;
                count += 1;
            }
        }
    }
    for s in count..NUM_SEGMENTS {
        quant[s] = quant[count - 1];
    }

    let uv_alpha = (uv_alpha_sum / total as i64) as i32;
    let dq_uv_ac =
        (uv_alpha - MID_UV_ALPHA) * (MAX_DQ_UV - MIN_DQ_UV) / (MAX_UV_ALPHA - MIN_UV_ALPHA);
    let dq_uv_ac = (dq_uv_ac * i32::from(strength) / 100).clamp(MIN_DQ_UV, MAX_DQ_UV);
    let dq_uv_dc = (-4 * i32::from(strength) / 100).clamp(-15, 15);

    SegmentPlan {
        map: mb_alpha.iter().map(|&a| remap[alpha_map[a]]).collect(),
        quant,
        count,
        uv_dc_delta: dq_uv_dc as i8,
        uv_ac_delta: dq_uv_ac as i8,
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::{ChromaDownsampling, ColorRange, Planar8};
    use gamut_core::{Dimensions, ImageRef, Rgb8};
    use libwebp_oracle::fixtures::{pattern_rgba, photo_like_rgba};
    use libwebp_oracle::{LibwebpLossyConfig, libwebp_encode_yuv_mb_quants};

    use super::*;
    use crate::backend::Vp8Planes;
    use crate::vp8::bool_coder::BoolDecoder;
    use crate::vp8::encoder::pad_plane;
    use crate::vp8::header::{self, UNCOMPRESSED_CHUNK_LEN};

    /// BT.601 limited-range planes of an RGBA fixture, as the lossy encoder converts them.
    fn planes(rgba: &[u8], w: u32, h: u32) -> Vp8Planes {
        let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
        Vp8Planes::try_from(Planar8::from_rgb8_bt601(
            img,
            ColorRange::Limited,
            ChromaDownsampling::Box,
        ))
        .unwrap()
    }

    /// Runs [`plan_segments`] over `yuv` padded the way the frame coder pads it.
    fn plan(yuv: &Vp8Planes, quality: u8, strength: u8, fast: bool) -> SegmentPlan {
        let (w, h) = (
            yuv.dimensions().width() as usize,
            yuv.dimensions().height() as usize,
        );
        let (mb_cols, mb_rows) = (w.div_ceil(16), h.div_ceil(16));
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let [y, u, v] = yuv.planes();
        let y = pad_plane(y, w, h, mb_cols * 16, mb_rows * 16);
        let u = pad_plane(u, cw, ch, mb_cols * 8, mb_rows * 8);
        let v = pad_plane(v, cw, ch, mb_cols * 8, mb_rows * 8);
        let source = Source {
            y: &y,
            u: &u,
            v: &v,
            y_stride: mb_cols * 16,
            c_stride: mb_cols * 8,
            mb_cols,
            mb_rows,
        };
        plan_segments(&source, quality, strength, fast)
    }

    /// Differential against libwebp: fed the same planes, libwebp codes every macroblock at the
    /// quantizer gamut's analysis and SNS give it, and its frame header carries the same chroma
    /// deltas. Sizes include partial macroblocks (the padded import) and single-row/-column grids
    /// (the missing-edge predictors); efforts 0-1 take the fast analysis; strengths 0, 50 and 100
    /// span off, default and full modulation.
    #[test]
    fn segment_quantizers_match_libwebp_per_macroblock() {
        for (w, h, rgba) in [
            (96u32, 80u32, photo_like_rgba(96, 80, 0x5e9)),
            (73, 41, photo_like_rgba(73, 41, 0x77)),
            (64, 48, pattern_rgba(64, 48)),
            (16, 120, photo_like_rgba(16, 120, 3)),
            (130, 16, photo_like_rgba(130, 16, 4)),
        ] {
            let yuv = planes(&rgba, w, h);
            for method in [0u8, 1, 2, 4, 6] {
                for (quality, strength) in [(75u8, 50u8), (20, 100), (90, 0), (50, 50)] {
                    let label = format!("{w}x{h} method {method} q{quality} sns {strength}");
                    let cfg = LibwebpLossyConfig {
                        quality: f32::from(quality),
                        filter_type: 1,
                        segments: 4,
                        method: i32::from(method),
                        filter_strength: 60,
                        sns_strength: i32::from(strength),
                    };
                    let (webp, lib_quant) = libwebp_encode_yuv_mb_quants(yuv.planes(), w, h, &cfg);
                    let plan = plan(&yuv, quality, strength, method <= 1);
                    let ours: Vec<u8> = plan.map.iter().map(|&s| plan.quant[s]).collect();
                    assert_eq!(ours, lib_quant, "per-macroblock quantizers, {label}");

                    let vp8 = &webp[20..];
                    let chunk = header::read_uncompressed_chunk(vp8).unwrap();
                    let end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
                    let mut modes = BoolDecoder::new(&vp8[UNCOMPRESSED_CHUNK_LEN..end]);
                    let (head, _) = header::read_frame_header(&chunk, &mut modes);
                    assert_eq!(
                        (head.quant.uv_dc_delta, head.quant.uv_ac_delta),
                        (plan.uv_dc_delta, plan.uv_ac_delta),
                        "chroma deltas, {label}"
                    );
                }
            }
        }
    }

    /// The unmodulated base quantizer is libwebp's `QualityToCompression` index: the endpoints,
    /// the default 75 (libwebp's 26), and saturation above 100.
    #[test]
    fn unmodulated_quantizer_follows_the_quality_curve() {
        let base = |q| segment_quant(quality_to_compression(q), 0.0, 0);
        assert_eq!(
            [base(0), base(50), base(75), base(100), base(255)],
            [127, 38, 26, 0, 0]
        );
        for q in 1u8..=100 {
            assert!(base(q) <= base(q - 1), "must be non-increasing at q={q}");
        }
    }
}
