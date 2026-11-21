//! The WebP `ALPH` alpha chunk (RFC 9649 §2.7.1): an 8-bit transparency plane stored alongside a
//! lossy `VP8 ` bitstream in an extended (`VP8X`) file.
//!
//! The plane is optionally **filtered** — each value predicted from its left, top, or
//! left+top−topleft (gradient) neighbor, the residual stored — which decorrelates it so the
//! lossless compressor packs it tighter. The filter is bijective and lossless regardless of how the
//! residuals are stored, so the alpha round-trips exactly either way. This module implements the
//! chunk header, the four filters, and both storage methods: **raw** (`C=0`) and **lossless** (`C=1`,
//! a headerless [`crate::vp8l`] image-stream carrying the residuals in its green channel).
//!
//! The one lossy step is optional and comes first: below alpha quality `100` the plane is
//! **level-reduced** (`P=1`) by a port of libwebp's `QuantizeLevels` before it is filtered, so what
//! the filters and coders then store exactly is the quantized plane.

use gamut_core::{Dimensions, Error, Result};
use gamut_dsp::math::clip_pixel8;

use crate::config::Effort;
use crate::vp8::effort::{AlphaEffort, EFFORT_TABLE};
use crate::vp8l::bit_io::BitReader;
use crate::vp8l::decoder::decode_image;
use crate::vp8l::encoder::encode_image;

/// Alpha filtering method (RFC 9649 §2.7.1 Figure 10, field `F`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlphaFilter {
    /// No filtering: the predictor is always 0, so residuals are the alpha values themselves.
    None,
    /// Horizontal: predict each value from the pixel to its left.
    Horizontal,
    /// Vertical: predict each value from the pixel above.
    Vertical,
    /// Gradient: predict from `clip(left + top − topleft)`.
    Gradient,
}

impl AlphaFilter {
    /// The 2-bit `F` field value.
    fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Horizontal => 1,
            Self::Vertical => 2,
            Self::Gradient => 3,
        }
    }

    /// Decodes the 2-bit `F` field.
    fn from_code(code: u8) -> Self {
        match code & 0x3 {
            1 => Self::Horizontal,
            2 => Self::Vertical,
            3 => Self::Gradient,
            _ => Self::None,
        }
    }

    /// The predictor for pixel `(x, y)` of a `width`-wide alpha `plane`, reading only already-known
    /// pixels (RFC 9649 §2.7.1). The top-left pixel predicts 0; the leftmost column of the horizontal
    /// and gradient filters predicts from the pixel above; the top row of the vertical and gradient
    /// filters predicts from the pixel to the left.
    fn predict(self, plane: &[u8], width: usize, x: usize, y: usize) -> u8 {
        let at = |x: usize, y: usize| plane[y * width + x];
        if x == 0 && y == 0 {
            return 0;
        }
        match self {
            Self::None => 0,
            Self::Horizontal => {
                if x > 0 {
                    at(x - 1, y)
                } else {
                    at(0, y - 1)
                }
            }
            Self::Vertical => {
                if y > 0 {
                    at(x, y - 1)
                } else {
                    at(x - 1, 0)
                }
            }
            Self::Gradient => {
                if x == 0 {
                    at(0, y - 1)
                } else if y == 0 {
                    at(x - 1, 0)
                } else {
                    let (a, b, c) = (
                        i32::from(at(x - 1, y)),
                        i32::from(at(x, y - 1)),
                        i32::from(at(x - 1, y - 1)),
                    );
                    clip_pixel8(a + b - c)
                }
            }
        }
    }
}

/// Filters an alpha `plane` (`width * height` bytes, scan order), returning the residuals
/// `(alpha − predictor) mod 256` (RFC 9649 §2.7.1). The predictor reads the original plane, which —
/// because the transform is lossless — equals the values the decoder will have reconstructed.
#[must_use]
pub(crate) fn filter(plane: &[u8], width: usize, height: usize, method: AlphaFilter) -> Vec<u8> {
    let mut out = vec![0u8; plane.len()];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            out[i] = plane[i].wrapping_sub(method.predict(plane, width, x, y));
        }
    }
    out
}

/// Inverts [`filter`]: reconstructs the alpha plane from `residuals`, predicting from the
/// already-reconstructed pixels (`alpha = (predictor + residual) mod 256`).
#[must_use]
pub(crate) fn unfilter(
    residuals: &[u8],
    width: usize,
    height: usize,
    method: AlphaFilter,
) -> Vec<u8> {
    let mut plane = vec![0u8; residuals.len()];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let pred = method.predict(&plane, width, x, y);
            plane[i] = pred.wrapping_add(residuals[i]);
        }
    }
    plane
}

/// Picks the alpha filter whose residuals have the least total magnitude — a cheap proxy for "most
/// compressible" (the choice only affects size, never correctness). Treating residuals as signed
/// (distance from 0 or 256) favors the smoothest predictor.
#[must_use]
pub(crate) fn choose_filter(plane: &[u8], width: usize, height: usize) -> AlphaFilter {
    [
        AlphaFilter::None,
        AlphaFilter::Horizontal,
        AlphaFilter::Vertical,
        AlphaFilter::Gradient,
    ]
    .into_iter()
    .min_by_key(|&m| {
        filter(plane, width, height, m)
            .iter()
            .map(|&r| u32::from(r.min(r.wrapping_neg())))
            .sum::<u32>()
    })
    .unwrap_or(AlphaFilter::None)
}

/// The `ALPH` header's pre-processing field `P` (RFC 9649 §2.7.1): what the encoder did to the
/// plane before filtering it. Informative only — a decoder may use level reduction as a hint to
/// dither, and gamut's decoder, like libwebp's by default, ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Preprocessing {
    /// `P=0`: the plane is stored as given.
    None = 0,
    /// `P=1`: the plane was level-reduced ([`quantize_levels`]) before filtering.
    LevelReduction = 1,
}

/// The `ALPH` header byte, `Rsv(2) P(2) F(2) C(2)` from the most significant bit down (RFC 9649
/// §2.7.1), with `Rsv = 0`.
///
/// The fields occupy disjoint bit lanes, so they are combined by addition rather than `|`: a `|`
/// would have an equivalent `^` twin no input can tell apart. That removes the twin and with it
/// the two `| → ^` mutants the earlier shift-and-or form generated.
fn header_byte(pre: Preprocessing, method: AlphaFilter, compression: u8) -> u8 {
    pre as u8 * 16 + method.code() * 4 + compression
}

/// The number of alpha levels libwebp keeps at `quality` (`alpha_enc.c`, `EncodeAlphaInternal`), or
/// `None` at `100` and above, where the plane is not quantized at all.
///
/// libwebp spreads `0..=70` over `2..=16` levels (`2 + quality / 5`) — 16 levels already give a low
/// error, so it calls that "moderate" — and `71..=99` over `24..=248` (`16 + (quality − 70) · 8`).
/// The two pieces meet at `70`, where both give 16, so the split is written as a sum over
/// `min`/`saturating_sub` rather than as a comparison whose `<=`/`<` twins would be equivalent.
fn alpha_levels(quality: u8) -> Option<usize> {
    (quality < 100)
        .then(|| 2 + usize::from(quality.min(70)) / 5 + usize::from(quality.saturating_sub(70)) * 8)
}

/// Reduces `plane` to at most `num_levels` distinct values (`2..=256`), keeping its minimum and
/// maximum: a port of libwebp's `QuantizeLevels` (`src/utils/quant_levels_utils.c`), so the same
/// plane at the same level count quantizes to the same bytes it does in libwebp.
///
/// It is a one-dimensional k-means over the value histogram: centroids start evenly spread between
/// the minimum and maximum, each value joins the nearest, the inner centroids move to their
/// members' mean, and this repeats up to six times or until the squared error stops improving by
/// more than `1e-4` per pixel. The two outer centroids stay pinned at the extremes, so a fully
/// transparent or fully opaque pixel keeps its value. A plane that already has no more than
/// `num_levels` values is left untouched.
///
/// Two guards of the C original are dropped because they cannot change the result: the per-value
/// `freq[s] > 0` test before accumulating (adding a zero count and a zero sum is exact), and the
/// `num_levels > 2` test before moving the inner centroids (the inner range is then empty).
pub(crate) fn quantize_levels(plane: &mut [u8], num_levels: usize) {
    /// libwebp's `MAX_ITER`.
    const MAX_ITER: usize = 6;

    let mut freq = [0u32; 256];
    for &v in plane.iter() {
        freq[usize::from(v)] += 1;
    }
    let present: Vec<usize> = (0..256).filter(|&s| freq[s] > 0).collect();
    let (Some(&min_s), Some(&max_s)) = (present.first(), present.last()) else {
        return;
    };
    if present.len() <= num_levels {
        return;
    }

    let mut inv_q_level = [0f64; 256];
    for (i, centroid) in inv_q_level.iter_mut().enumerate().take(num_levels) {
        *centroid = min_s as f64 + (max_s - min_s) as f64 * i as f64 / (num_levels - 1) as f64;
    }
    let mut q_level = [0usize; 256];
    let mut last_err = 1e38;
    for _ in 0..MAX_ITER {
        let mut q_sum = [0f64; 256];
        let mut q_count = [0f64; 256];
        // Twice the midpoint between each pair of neighbouring centroids, summed exactly as the C
        // original sums them inside its scan.
        let bounds: Vec<f64> = inv_q_level[..num_levels]
            .windows(2)
            .map(|pair| pair[0] + pair[1])
            .collect();
        let mut slot = 0;
        for s in min_s..=max_s {
            // Move up past every boundary `s` lies above. The C original's `while (…) ++slot;`
            // becomes a count over the remaining boundaries, so the scan is bounded by the slice
            // it walks: a mutated step cannot spin it forever.
            slot += bounds[slot..]
                .iter()
                .take_while(|&&bound| (2 * s) as f64 > bound)
                .count();
            q_sum[slot] += (s * freq[s] as usize) as f64;
            q_count[slot] += f64::from(freq[s]);
            q_level[s] = slot;
        }
        for slot in 1..num_levels - 1 {
            if q_count[slot] > 0.0 {
                inv_q_level[slot] = q_sum[slot] / q_count[slot];
            }
        }
        let err = squared_error(&freq, &inv_q_level, &q_level, min_s, max_s);
        if converged(last_err, err, plane.len()) {
            break;
        }
        last_err = err;
    }

    let mut map = [0u8; 256];
    for s in min_s..=max_s {
        // `inv_q_level` stays within `min_s..=max_s`, so `+ 0.5` then truncation is C's rounding.
        map[s] = (inv_q_level[q_level[s]] + 0.5) as u8;
    }
    for v in plane.iter_mut() {
        *v = map[usize::from(*v)];
    }
}

/// The squared error of one `quantize_levels` pass, summed as libwebp's `QuantizeLevels` sums it:
/// `Σ freq[s] · (s − centroids[q_level[s]])²` over the values `min_s..=max_s`.
fn squared_error(
    freq: &[u32; 256],
    centroids: &[f64; 256],
    q_level: &[usize; 256],
    min_s: usize,
    max_s: usize,
) -> f64 {
    let mut err = 0.0;
    for s in min_s..=max_s {
        let error = s as f64 - centroids[q_level[s]];
        err += f64::from(freq[s]) * error * error;
    }
    err
}

/// libwebp's `QuantizeLevels` stopping rule, `if (last_err - err < ERROR_THRESHOLD * N) break;`:
/// the k-means stops once a pass improves the squared error over `pixels` values by less than
/// `1e-4` per pixel.
fn converged(last_err: f64, err: f64, pixels: usize) -> bool {
    /// libwebp's `ERROR_THRESHOLD`, per pixel.
    const ERROR_THRESHOLD: f64 = 1e-4;
    last_err - err < ERROR_THRESHOLD * pixels as f64
}

/// Builds an `ALPH` chunk payload (RFC 9649 §2.7.1) for a **raw** (uncompressed) alpha plane: the
/// header byte (`Rsv=0`, `P=pre`, the filter method, `C=0`) followed by the filtered residuals.
#[must_use]
pub(crate) fn write_raw_alph(
    plane: &[u8],
    width: usize,
    height: usize,
    pre: Preprocessing,
) -> Vec<u8> {
    let method = choose_filter(plane, width, height);
    let mut out = Vec::with_capacity(1 + plane.len());
    out.push(header_byte(pre, method, 0)); // C = 0 (no compression)
    out.extend_from_slice(&filter(plane, width, height, method));
    out
}

/// Builds a **lossless-compressed** `ALPH` chunk payload (compression method `C=1`): the alpha
/// values — optionally pre-filtered by `method` — are placed in the green channel of an opaque ARGB
/// image and encoded as a headerless VP8L image-stream (RFC 9649 §2.7.1).
///
/// The pre-filter and the compression method are independent fields in the chunk header, and the
/// decoder unfilters after decompressing either way. Applying one is not obviously a win — the VP8L
/// spatial predictors already decorrelate — which is exactly why the choice is searched rather than
/// assumed at the top of the effort ladder.
///
/// # Errors
///
/// Propagates a VP8L encoding error (only a dimension mismatch, which cannot occur here).
fn write_compressed_alph(
    plane: &[u8],
    width: usize,
    height: usize,
    pre: Preprocessing,
    method: AlphaFilter,
    effort: Effort,
) -> Result<Vec<u8>> {
    let residuals = filter(plane, width, height, method);
    // Opaque alpha (bits 24-31) plus the residual in green (bits 8-15): disjoint lanes, added
    // rather than or-ed for the reason `header_byte` gives.
    let argb: Vec<u32> = residuals
        .iter()
        .map(|&a| 0xff00_0000 + (u32::from(a) << 8))
        .collect();
    let dims = Dimensions::new(width as u32, height as u32)?;
    let stream = encode_image(&argb, dims, effort)?;
    let mut out = Vec::with_capacity(1 + stream.len());
    out.push(header_byte(pre, method, 1)); // C = 1 (lossless)
    out.extend_from_slice(&stream);
    Ok(out)
}

/// Builds the smaller of the raw and lossless-compressed `ALPH` payloads, spending `effort` on the
/// compressed path's VP8L encode.
///
/// At `alpha_quality` `100` and above the alpha plane is stored **losslessly at every effort
/// level** — the knob only chooses how hard the lossless coder searches. Below `100` the plane is
/// first reduced to at most [`alpha_levels`]`(alpha_quality)` values by [`quantize_levels`] and the
/// header says so (`P=1`), exactly as libwebp's `alpha_quality` does; the reduced plane is then
/// stored losslessly the same way.
///
/// # Errors
///
/// Propagates a VP8L encoding error from the compressed path.
pub fn write_alph_with_effort(
    plane: &[u8],
    width: usize,
    height: usize,
    effort: Effort,
    alpha_quality: u8,
) -> Result<Vec<u8>> {
    let reduced;
    let (plane, pre) = match alpha_levels(alpha_quality) {
        Some(levels) => {
            let mut quantized = plane.to_vec();
            quantize_levels(&mut quantized, levels);
            reduced = quantized;
            (reduced.as_slice(), Preprocessing::LevelReduction)
        }
        None => (plane, Preprocessing::None),
    };
    let mut best = write_raw_alph(plane, width, height, pre);
    // A zero-sized plane has no `Dimensions` for the VP8L encode, and no compressed payload could
    // undercut its one-byte raw one anyway.
    if width == 0 || height == 0 {
        return Ok(best);
    }
    // The compressed path's pre-filter is a free choice the decoder inverts either way. Most rungs
    // leave it off and let the VP8L spatial predictors do the decorrelation; the top rungs search
    // all four, which costs a full VP8L encode each and is why it sits where it does on the ladder.
    let methods: &[AlphaFilter] = match EFFORT_TABLE[effort.level() as usize].alpha {
        AlphaEffort::Balanced => &[AlphaFilter::None],
        AlphaEffort::Exhaustive => &[
            AlphaFilter::None,
            AlphaFilter::Horizontal,
            AlphaFilter::Vertical,
            AlphaFilter::Gradient,
        ],
    };
    for &method in methods {
        let candidate = write_compressed_alph(plane, width, height, pre, method, effort)?;
        if candidate.len() < best.len() {
            best = candidate;
        }
    }
    Ok(best)
}

/// Decodes an `ALPH` chunk payload into the alpha plane (RFC 9649 §2.7.1), handling both the raw
/// (`C=0`) and lossless-compressed (`C=1`) storage methods. Compressed alpha is a headerless VP8L
/// image-stream whose green channel holds the filtered residuals; either way the filter `F` is then
/// inverted.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for an empty
/// payload, a raw payload of the wrong length, or a malformed compressed stream.
pub fn read_alph(payload: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let &header = payload
        .first()
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "ALPH: empty chunk"))?;
    let method = AlphaFilter::from_code(header >> 2);
    let data = &payload[1..];
    let residuals = match header & 0x3 {
        0 => {
            if data.len() != width * height {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ALPH: raw alpha length mismatch",
                ));
            }
            data.to_vec()
        }
        1 => {
            let mut r = BitReader::new(data);
            let argb = decode_image(&mut r, width as u32, height as u32)?;
            argb.iter().map(|&p| (p >> 8) as u8).collect() // green channel
        }
        _ => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ALPH: reserved compression method",
            ));
        }
    };
    Ok(unfilter(&residuals, width, height, method))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: usize, height: usize) -> Vec<u8> {
        (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                ((x * 9 + y * 5 + (x ^ y) * 3) & 0xff) as u8
            })
            .collect()
    }

    #[test]
    fn each_filter_inverts_exactly() {
        let (w, h) = (23, 17);
        let plane = pattern(w, h);
        for m in [
            AlphaFilter::None,
            AlphaFilter::Horizontal,
            AlphaFilter::Vertical,
            AlphaFilter::Gradient,
        ] {
            let residuals = filter(&plane, w, h, m);
            assert_eq!(
                unfilter(&residuals, w, h, m),
                plane,
                "filter {m:?} round-trip"
            );
        }
    }

    #[test]
    fn none_filter_stores_alpha_verbatim() {
        let plane = pattern(8, 8);
        assert_eq!(filter(&plane, 8, 8, AlphaFilter::None), plane);
    }

    #[test]
    fn raw_alph_chunk_round_trips() {
        let (w, h) = (19, 11);
        let plane = pattern(w, h);
        let chunk = write_raw_alph(&plane, w, h, Preprocessing::None);
        assert_eq!(chunk.len(), 1 + w * h);
        assert_eq!(chunk[0] & 0x3, 0, "compression method is raw");
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
    }

    /// A plane with either side zero has no `Dimensions` for the VP8L encode, so it takes the raw
    /// payload; one zero side alone must suffice, or the compressed path would fail on it.
    #[test]
    fn zero_sided_planes_store_the_raw_payload() {
        for (w, h) in [(0, 5), (5, 0)] {
            assert_eq!(
                write_alph_with_effort(&[], w, h, Effort::default(), 100).unwrap(),
                write_raw_alph(&[], w, h, Preprocessing::None),
            );
        }
    }

    /// The filter method lives in bits 2-3 of the `ALPH` header byte (RFC 9649 §2.7.1), beside the
    /// compression method in bits 0-1. Writing it into the wrong bits still produces a chunk the
    /// reader accepts — it just unfilters with the wrong method, so the plane comes back wrong.
    /// Every method is round-tripped through the compressed writer with a plane that is not
    /// filter-invariant, which is what makes a misplaced field observable.
    #[test]
    fn every_alpha_filter_survives_the_compressed_round_trip() {
        let (w, h) = (12usize, 9usize);
        // Gradients in both axes plus a corner discontinuity: no two filters agree on this plane.
        let plane: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                ((x * 9 + y * 23) % 256) as u8
            })
            .collect();
        for method in [
            AlphaFilter::None,
            AlphaFilter::Horizontal,
            AlphaFilter::Vertical,
            AlphaFilter::Gradient,
        ] {
            let payload =
                write_compressed_alph(&plane, w, h, Preprocessing::None, method, Effort::default())
                    .expect("the plane matches the dimensions");
            assert_eq!(
                payload[0] & 0x03,
                0x01,
                "compression method must be lossless for {method:?}"
            );
            assert_eq!(
                (payload[0] >> 2) & 0x03,
                method.code(),
                "filter method must occupy bits 2-3 for {method:?}"
            );
            let back = read_alph(&payload, w, h).expect("round trip");
            assert_eq!(back, plane, "{method:?} did not round-trip");
        }
    }

    #[test]
    fn read_alph_rejects_bad_input() {
        assert!(read_alph(&[], 4, 4).is_err());
        assert!(read_alph(&[0, 1, 2], 4, 4).is_err(), "wrong raw length");
        assert!(
            read_alph(&[0x01, 0x00], 8, 8).is_err(),
            "truncated compressed stream"
        );
    }

    #[test]
    fn compressed_alph_round_trips() {
        let (w, h) = (20, 12);
        let plane = pattern(w, h);
        let chunk = write_compressed_alph(
            &plane,
            w,
            h,
            Preprocessing::None,
            AlphaFilter::None,
            Effort::default(),
        )
        .unwrap();
        assert_eq!(chunk[0] & 0x3, 1, "compression method is lossless");
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
    }

    #[test]
    fn from_code_maps_each_filter() {
        // Pin the F-field decode: a deleted match arm would silently fall back to `None`.
        assert_eq!(AlphaFilter::from_code(0), AlphaFilter::None);
        assert_eq!(AlphaFilter::from_code(1), AlphaFilter::Horizontal);
        assert_eq!(AlphaFilter::from_code(2), AlphaFilter::Vertical);
        assert_eq!(AlphaFilter::from_code(3), AlphaFilter::Gradient);
        for m in [
            AlphaFilter::None,
            AlphaFilter::Horizontal,
            AlphaFilter::Vertical,
            AlphaFilter::Gradient,
        ] {
            assert_eq!(AlphaFilter::from_code(m.code()), m);
        }
    }

    #[test]
    fn filter_residuals_are_exact_per_method() {
        // Absolute residuals for a known 2×2 plane pin the predictor arithmetic, which the
        // filter↔unfilter round-trip cannot (a buggy predictor cancels in both directions).
        let plane = [100u8, 150, 120, 200]; // row-major, width 2
        assert_eq!(
            filter(&plane, 2, 2, AlphaFilter::None),
            [100, 150, 120, 200]
        );
        assert_eq!(
            filter(&plane, 2, 2, AlphaFilter::Horizontal),
            [100, 50, 20, 80]
        );
        assert_eq!(
            filter(&plane, 2, 2, AlphaFilter::Vertical),
            [100, 50, 20, 50]
        );
        assert_eq!(
            filter(&plane, 2, 2, AlphaFilter::Gradient),
            [100, 50, 20, 30]
        );
    }

    #[test]
    fn write_alph_picks_the_smaller_and_round_trips() {
        // A smoothly-banded plane compresses well, so the chosen payload should beat the raw size.
        let (w, h) = (64, 64);
        let plane: Vec<u8> = (0..w * h).map(|i| ((i / w) * 4) as u8).collect();
        let chunk = write_alph_with_effort(&plane, w, h, Effort::default(), 100).unwrap();
        assert!(
            chunk.len() < 1 + w * h,
            "compressible alpha should beat raw"
        );
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
    }

    #[test]
    fn alph_lossless_decode_survives_malformed() {
        // The headerless VP8L path that backs compressed (`C=1`) `ALPH` alpha: `read_alph` must
        // survive a garbage payload (advertising compression, then random bytes) without
        // panicking, for a range of declared dimensions.
        for &(w, h) in &[(1usize, 1usize), (16, 16), (24, 18), (33, 7)] {
            for seed in 0..128u32 {
                // Fuzz the ALPH header byte (covers raw `C=0`, VP8L `C=1`, and the reserved
                // methods, plus every filter) followed by random body bytes — the C=1 path runs the
                // headerless VP8L decoder on arbitrary prefix/transform/LZ77 data, which must
                // error, not panic.
                let mut payload = vec![(seed & 0xff) as u8];
                payload.extend(pseudo_random_bytes(seed ^ 0x1234, 48));
                let _ = read_alph(&payload, w, h);
            }
        }
    }

    /// A soft-edged alpha plane — a radial shadow fading from opaque at the centre to transparent
    /// at the corners, every step of the ramp in use — the content alpha quality exists for.
    fn soft_alpha(width: usize, height: usize) -> Vec<u8> {
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        let reach = cx.hypot(cy);
        (0..width * height)
            .map(|i| {
                let (x, y) = ((i % width) as f64 + 0.5, (i / width) as f64 + 0.5);
                (255.0 * (1.0 - (x - cx).hypot(y - cy) / reach)).round() as u8
            })
            .collect()
    }

    /// [`soft_alpha`] with ±16 of sensor-like grain on every pixel.
    fn grainy_alpha(width: usize, height: usize) -> Vec<u8> {
        let noise = pseudo_random_bytes(0x0a1f, width * height);
        soft_alpha(width, height)
            .iter()
            .zip(&noise)
            .map(|(&a, &n)| (i32::from(a) + i32::from(n % 33) - 16).clamp(0, 255) as u8)
            .collect()
    }

    /// The distinct values of `plane`, ascending.
    fn levels_of(plane: &[u8]) -> Vec<u8> {
        let mut levels = plane.to_vec();
        levels.sort_unstable();
        levels.dedup();
        levels
    }

    /// libwebp's level count at each boundary of its two-piece mapping (`alpha_enc.c`), and none at
    /// or above `100`.
    #[test]
    fn alpha_levels_follow_libwebps_mapping() {
        for (quality, levels) in [
            (0u8, Some(2usize)),
            (4, Some(2)),
            (5, Some(3)),
            (69, Some(15)),
            (70, Some(16)),
            (71, Some(24)),
            (99, Some(248)),
            (100, None),
            (u8::MAX, None),
        ] {
            assert_eq!(alpha_levels(quality), levels, "quality {quality}");
        }
    }

    /// The pre-processing field is bits 4-5 of the header, above `F` (2-3) and `C` (0-1).
    #[test]
    fn header_byte_places_each_field() {
        assert_eq!(header_byte(Preprocessing::None, AlphaFilter::None, 0), 0x00);
        assert_eq!(
            header_byte(Preprocessing::LevelReduction, AlphaFilter::None, 0),
            0x10
        );
        assert_eq!(
            header_byte(Preprocessing::LevelReduction, AlphaFilter::Gradient, 1),
            0x1d
        );
        assert_eq!(
            header_byte(Preprocessing::None, AlphaFilter::Horizontal, 1),
            0x05
        );
    }

    /// A plane with no more values than the level budget is returned as is, whatever its values —
    /// including at exactly the budget, where running the k-means anyway would fold `1` into the
    /// `0` centroid (the evenly spread starting centroids are `0`, `127.5` and `255`).
    #[test]
    fn quantize_levels_leaves_a_plane_within_budget_untouched() {
        let mut plane = vec![0u8, 1, 255, 1, 0];
        quantize_levels(&mut plane, 3);
        assert_eq!(plane, [0, 1, 255, 1, 0]);
        let mut empty: Vec<u8> = Vec::new();
        quantize_levels(&mut empty, 2);
        assert!(empty.is_empty());
    }

    /// A value exactly on the boundary between two centroids stays with the lower one, as the C
    /// original's strict `2 * s > c[k] + c[k + 1]` decides: over `0..=4` at three levels the
    /// starting centroids `0`, `2`, `4` put `1` and `3` on boundaries, so they join `0` and `2`
    /// and the middle centroid settles at `2.5`, which rounds to `3`.
    #[test]
    fn quantize_levels_keeps_a_tie_with_the_lower_centroid() {
        let mut plane = vec![0u8, 1, 2, 3, 4];
        quantize_levels(&mut plane, 3);
        assert_eq!(plane, [0, 0, 3, 3, 4]);
    }

    /// An inner centroid no value is nearest to keeps its position rather than becoming `0 / 0`:
    /// with three levels over `{0, 1, 2, 255}` the middle one (127.5) attracts nothing, and a NaN
    /// there would stop every value from reaching the top centroid.
    #[test]
    fn quantize_levels_keeps_an_empty_centroid() {
        let mut plane = vec![0u8, 1, 2, 255];
        quantize_levels(&mut plane, 3);
        assert_eq!(plane, [0, 0, 0, 255]);
    }

    /// The pass error weights each value's squared distance from its centroid by its count:
    /// `3 · (1 − 0)² + 1 · (4 − 2)² = 7`. The `4` term has a distance other than `1`, so it tells
    /// `freq · e · e` from `freq / e · e` and `freq · e / e`; the absent `2` and `3` add nothing.
    #[test]
    fn squared_error_weights_each_value_by_its_count() {
        let mut freq = [0u32; 256];
        freq[1] = 3;
        freq[4] = 1;
        let mut centroids = [0f64; 256];
        centroids[1] = 2.0;
        let mut q_level = [0usize; 256];
        q_level[4] = 1;
        assert_eq!(squared_error(&freq, &centroids, &q_level, 1, 4), 7.0);
    }

    /// The k-means stops on an improvement strictly below `1e-4` per pixel: no improvement and
    /// half the threshold stop it, an improvement of exactly the threshold does not.
    #[test]
    fn converged_stops_strictly_below_the_per_pixel_threshold() {
        let pixels = 10_000;
        let threshold = 1e-4 * pixels as f64;
        assert!(converged(5.0, 5.0, pixels), "no improvement");
        assert!(
            converged(threshold, threshold / 2.0, pixels),
            "half the threshold"
        );
        assert!(!converged(threshold, 0.0, pixels), "exactly the threshold");
    }

    /// Below alpha quality 100 the header records level reduction and the stored plane has at most
    /// the implied number of values, keeping the extremes; at 100 the plane is exact and `P = 0`.
    #[test]
    fn write_alph_level_reduces_below_quality_100() {
        let (w, h) = (40, 32);
        let plane = soft_alpha(w, h);
        for quality in [0u8, 30, 70, 90, 99] {
            let chunk = write_alph_with_effort(&plane, w, h, Effort::default(), quality).unwrap();
            assert_eq!(chunk[0] >> 4, 1, "P = 1 at quality {quality}");
            let back = read_alph(&chunk, w, h).unwrap();
            let levels = levels_of(&back);
            assert!(
                levels.len() <= alpha_levels(quality).unwrap(),
                "quality {quality}: {} levels",
                levels.len()
            );
            assert_eq!(levels.first(), levels_of(&plane).first(), "minimum kept");
            assert_eq!(levels.last(), levels_of(&plane).last(), "maximum kept");
        }
        let chunk = write_alph_with_effort(&plane, w, h, Effort::default(), 100).unwrap();
        assert_eq!(chunk[0] >> 4, 0, "P = 0 at quality 100");
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
    }

    /// On a grainy soft-alpha fixture — the radial shadow with ±16 of sensor-like noise, the kind of
    /// mask whose lossless `ALPH` dominates a file — the payload is non-increasing as alpha quality
    /// falls from lossless through `75` (56 levels) down to `0` (issue #1206's acceptance
    /// criterion, as corrected on the issue to the range libwebp itself satisfies). The qualities
    /// visit every distinct level count in that range once: `71..=75` one by one, then every fifth
    /// quality, since `2 + quality / 5` is constant in between.
    ///
    /// This is a claim about the fixture, not a law of the quantizer, and the range is where it
    /// holds. The k-means levels at one count are not nested in those at the next, so between `76`
    /// and `99` (64 to 248 levels — more than this plane's structure needs) the size wanders within
    /// about 2% of lossless rather than falling; and on a perfectly smooth ramp (`soft_alpha`
    /// alone) the quantized steps predict worse than the ramp did, so mid qualities there cost more
    /// than lossless. libwebp behaves identically: the plane is byte-for-byte its own.
    #[test]
    fn alph_size_is_non_increasing_as_alpha_quality_falls() {
        let (w, h) = (64, 48);
        let plane = grainy_alpha(w, h);
        let sizes: Vec<usize> = [100u8, 75, 74, 73, 72, 71]
            .into_iter()
            .chain((0..=70u8).rev().step_by(5))
            .map(|q| {
                write_alph_with_effort(&plane, w, h, Effort::default(), q)
                    .unwrap()
                    .len()
            })
            .collect();
        assert!(
            sizes.windows(2).all(|pair| pair[1] <= pair[0]),
            "ALPH sizes from quality 100 down: {sizes:?}"
        );
        assert!(
            sizes[sizes.len() - 1] < sizes[0],
            "quality 0 must actually shrink the payload: {sizes:?}"
        );
    }

    /// Differential against libwebp's `QuantizeLevels`: libwebp level-reduces the alpha plane at
    /// `alpha_quality < 100` and then codes it losslessly, so the alpha it decodes from its own
    /// file is its quantized plane, which [`quantize_levels`] must reproduce byte for byte. The
    /// qualities straddle both pieces of the level mapping and the fixtures both a smooth ramp and
    /// noise, which keeps the k-means iterating for its full budget.
    #[test]
    fn quantize_levels_matches_libwebp() {
        use libwebp_oracle::{libwebp_decode_rgba, libwebp_encode_lossy_rgba_alpha_quality};
        let (w, h) = (40usize, 32usize);
        for (label, w, h, plane) in [
            ("soft", w, h, soft_alpha(w, h)),
            ("noise", w, h, pseudo_random_bytes(0x1206, w * h)),
            ("grainy", 160, 120, grainy_alpha(160, 120)),
        ] {
            let rgba: Vec<u8> = plane.iter().flat_map(|&a| [90, 140, 200, a]).collect();
            for quality in [0u8, 3, 22, 37, 69, 70, 71, 85, 99] {
                let webp = libwebp_encode_lossy_rgba_alpha_quality(
                    &rgba,
                    w as u32,
                    h as u32,
                    75.0,
                    i32::from(quality),
                );
                let libwebp: Vec<u8> = libwebp_decode_rgba(&webp)
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| p[3])
                    .collect();
                let mut ours = plane.clone();
                quantize_levels(&mut ours, alpha_levels(quality).unwrap());
                assert_eq!(ours, libwebp, "{label} at alpha quality {quality}");
            }
        }
    }

    /// A cheap deterministic byte generator (an LCG), so the corpus above is reproducible.
    fn pseudo_random_bytes(seed: u32, len: usize) -> Vec<u8> {
        let mut state = seed ^ 0x9e37_79b9;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }
}
