//! The WebP `ALPH` alpha chunk (RFC 9649 §2.7.1): an 8-bit transparency plane stored alongside a
//! lossy `VP8 ` bitstream in an extended (`VP8X`) file.
//!
//! The plane is optionally **filtered** — each value predicted from its left, top, or
//! left+top−topleft (gradient) neighbor, the residual stored — which decorrelates it so the
//! lossless compressor packs it tighter. The filter is bijective and lossless regardless of how the
//! residuals are stored, so the alpha round-trips exactly either way. This module implements the
//! chunk header, the four filters, and both storage methods: **raw** (`C=0`) and **lossless** (`C=1`,
//! a headerless [`crate::vp8l`] image-stream carrying the residuals in its green channel).

use gamut_color::clip_pixel8;
use gamut_core::{Dimensions, Error, Result};

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

/// Builds an `ALPH` chunk payload (RFC 9649 §2.7.1) for a **raw** (uncompressed) alpha plane: the
/// header byte (`Rsv=0`, `P=0`, the filter method, `C=0`) followed by the filtered residuals.
#[must_use]
pub(crate) fn write_raw_alph(plane: &[u8], width: usize, height: usize) -> Vec<u8> {
    let method = choose_filter(plane, width, height);
    let mut out = Vec::with_capacity(1 + plane.len());
    out.push(method.code() << 2); // P = 0, C = 0 (no compression)
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
    method: AlphaFilter,
    effort: Effort,
) -> Result<Vec<u8>> {
    let residuals = filter(plane, width, height, method);
    let argb: Vec<u32> = residuals
        .iter()
        .map(|&a| 0xff00_0000 | (u32::from(a) << 8))
        .collect();
    let dims = Dimensions {
        width: width as u32,
        height: height as u32,
    };
    let stream = encode_image(&argb, dims, effort)?;
    let mut out = Vec::with_capacity(1 + stream.len());
    out.push(method.code() << 2 | 0x01); // P = 0, C = 1 (lossless)
    out.extend_from_slice(&stream);
    Ok(out)
}

/// Builds the smaller of the raw and lossless-compressed `ALPH` payloads for an alpha plane.
///
/// # Errors
///
/// Propagates a VP8L encoding error from the compressed path.
pub fn write_alph(plane: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    write_alph_with_effort(plane, width, height, Effort::default())
}

/// Builds the smaller of the raw and lossless-compressed `ALPH` payloads, spending `effort` on the
/// compressed path's VP8L encode.
///
/// The alpha plane is stored **losslessly at every effort level** — the knob only chooses how hard
/// the lossless coder searches.
///
/// # Errors
///
/// Propagates a VP8L encoding error from the compressed path.
pub fn write_alph_with_effort(
    plane: &[u8],
    width: usize,
    height: usize,
    effort: Effort,
) -> Result<Vec<u8>> {
    let mut best = write_raw_alph(plane, width, height);
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
        let candidate = write_compressed_alph(plane, width, height, method, effort)?;
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
/// Returns [`Error::InvalidInput`] for an empty payload, a raw payload of the wrong length, or a
/// malformed compressed stream.
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
        let chunk = write_raw_alph(&plane, w, h);
        assert_eq!(chunk.len(), 1 + w * h);
        assert_eq!(chunk[0] & 0x3, 0, "compression method is raw");
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
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
            let payload = write_compressed_alph(&plane, w, h, method, Effort::default())
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
        let chunk =
            write_compressed_alph(&plane, w, h, AlphaFilter::None, Effort::default()).unwrap();
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
        let chunk = write_alph(&plane, w, h).unwrap();
        assert!(
            chunk.len() < 1 + w * h,
            "compressible alpha should beat raw"
        );
        assert_eq!(read_alph(&chunk, w, h).unwrap(), plane);
    }
}
