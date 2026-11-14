//! VP8 key-frame reconstruction pipeline (RFC 6386 §10–§14): the macroblock loop that ties together
//! prediction, the transforms, quantization, and token coding into an encodable/decodable frame.
//!
//! This is the keystone of the lossy path. Each macroblock is predicted from the **reconstructed**
//! neighbors in a recon buffer (the encoder predicts exactly as the decoder does), so the encoder's
//! reconstruction is bit-identical to any conformant decoder's output. Luma uses whole-block 16×16
//! DC/V/H/TM **or** per-4×4 `B_PRED` (ten directional submodes), and chroma whole-block 8×8 DC/V/H/TM;
//! the encoder picks the lowest-SAD candidate per macroblock. A whole-block macroblock carries a Y2
//! (luma-DC WHT) block; a `B_PRED` one codes luma DC inline (plane 3). The reconstruction is deblocked
//! by the simple or normal loop filter as a final pass. Tokens may be split across 1/2/4/8 partitions
//! by macroblock row, and all-zero macroblocks are coded as skipped. Per-macroblock loop-filter
//! adjustments (RFC 6386 §9.4) shift each macroblock's filter level. STATUS.md section L.
//!
//! The pipeline is split by direction: `encoder` decides and writes a frame
//! ([`encode_frame_filtered`]), `decoder` reads one ([`decode_frame`]), and `reconstruct` owns the
//! `FrameBuffers` reconstruction both of them drive. This module is the crate-internal entry point
//! that re-exports them, plus the macroblock syntax both directions thread identically: the
//! segment-id tree, the non-zero coefficient contexts, and the `B_PRED` submode contexts.
//!
//! Its test modules include the libwebp differential suite at the YUV-plane level
//! (`oracle_tests`) and the frame decoder's robustness corpus (`robustness_tests`): both drive the
//! frame coder directly, which nothing outside the crate can reach.

// The macroblock/block math indexes several fixed-size arrays in lock-step (and over partial ranges
// like `1..16`), where explicit indices read closer to the spec than iterator adaptors.
#![allow(clippy::needless_range_loop)]

pub(crate) use super::decoder::decode_frame;
#[cfg(test)]
pub(crate) use super::encoder::encode_frame;
pub(crate) use super::encoder::{EncodeOptions, encode_frame_filtered};
/// Re-exported so the frame tests carry the loop-filter delta type next to [`EncodeOptions`].
#[cfg(test)]
pub(crate) use super::header::LoopFilterDeltas;
use super::prediction;

/// Segment-id coding tree (RFC 6386 §10 `mb_segment_tree`): four leaves over two boolean decisions.
pub(super) const MB_SEGMENT_TREE: &[i8] = &[2, 4, 0, -1, -2, -3];

/// Per-macroblock-column entropy context: whether the prior block in each position carried at least
/// one non-zero coefficient (RFC 6386 §13.3). A single instance also serves as the running "left"
/// context, reset at the start of each macroblock row.
#[derive(Clone, Copy, Default)]
pub(super) struct EntropyCtx {
    /// Y2 (luma-DC WHT) block.
    pub(super) y2: bool,
    /// The four luma sub-block columns (above) / rows (left).
    pub(super) y: [bool; 4],
    /// The two U sub-block columns / rows.
    pub(super) u: [bool; 2],
    /// The two V sub-block columns / rows.
    pub(super) v: [bool; 2],
}

/// One macroblock's quantized coefficient levels: the Y2 block, 16 luma sub-blocks, 4 U and 4 V.
#[derive(Clone, Default)]
pub(super) struct MbLevels {
    pub(super) y2: [i16; 16],
    pub(super) y: [[i16; 16]; 16],
    pub(super) u: [[i16; 16]; 4],
    pub(super) v: [[i16; 16]; 4],
}

/// Whether a macroblock carries any non-zero quantized coefficient — the second half of the
/// loop-filter interior-edge skip rule (RFC 6386 §15.1).
pub(super) fn mb_has_coeffs(levels: &MbLevels) -> bool {
    levels.y2.iter().any(|&x| x != 0)
        || levels.y.iter().flatten().any(|&x| x != 0)
        || levels.u.iter().flatten().any(|&x| x != 0)
        || levels.v.iter().flatten().any(|&x| x != 0)
}

/// Resets a macroblock's coefficient context to "no non-zero coefficients" for a skipped macroblock
/// (RFC 6386 §11.1): equivalent to coding all-zero blocks, but the `B_PRED` Y2 context persists since
/// such a macroblock carries no Y2 block.
pub(super) fn clear_mb_context(above: &mut EntropyCtx, left: &mut EntropyCtx, is_bpred: bool) {
    if !is_bpred {
        above.y2 = false;
        left.y2 = false;
    }
    above.y = [false; 4];
    left.y = [false; 4];
    above.u = [false; 2];
    left.u = [false; 2];
    above.v = [false; 2];
    left.v = [false; 2];
}

/// The above/left subblock-mode context for the `j`th subblock (RFC 6386 §11.3): the mode of the
/// subblock above (within the macroblock for rows > 0, else `above_col`) and to the left (within for
/// columns > 0, else `left_col`).
pub(super) fn bmode_context(
    sub_modes: &[usize; 16],
    above_col: &[usize; 4],
    left_col: &[usize; 4],
    i: usize,
) -> (usize, usize) {
    let (r, c) = (i / 4, i % 4);
    let a = if r > 0 {
        sub_modes[i - 4]
    } else {
        above_col[c]
    };
    let l = if c > 0 { sub_modes[i - 1] } else { left_col[r] };
    (a, l)
}

/// The macroblock's bottom-row and right-column subblock modes, to seed the above/left context of the
/// next row/column (RFC 6386 §11.3 caveat 4): the actual submodes for `B_PRED`, else the constant
/// derived from the whole-block luma mode.
pub(super) fn bmode_propagation(
    is_bpred: bool,
    luma_mode: usize,
    sub_modes: &[usize; 16],
) -> ([usize; 4], [usize; 4]) {
    if is_bpred {
        (
            [sub_modes[12], sub_modes[13], sub_modes[14], sub_modes[15]],
            [sub_modes[3], sub_modes[7], sub_modes[11], sub_modes[15]],
        )
    } else {
        let bm = prediction::bmode_for_luma(luma_mode);
        ([bm; 4], [bm; 4])
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    //! Synthetic 4:2:0 frames shared by the encoder, decoder and round-trip suites.

    use gamut_color::{ChromaSubsampling, Planar8};
    use gamut_core::Dimensions;

    /// 4:2:0 chroma plane dimensions for a `width`×`height` luma plane, as `usize`.
    pub(in crate::vp8) fn chroma_dims(width: u32, height: u32) -> (usize, usize) {
        let (cw, ch) = ChromaSubsampling::Cs420.chroma_dimensions(width, height);
        (cw as usize, ch as usize)
    }

    /// Assembles a 4:2:0 `Planar8` from its three planes.
    pub(in crate::vp8) fn yuv420(
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    ) -> Planar8 {
        Planar8::from_planes_subsampled(
            Dimensions::new(width, height).unwrap(),
            ChromaSubsampling::Cs420,
            [y, u, v],
        )
        .unwrap()
    }

    /// Builds a 4:2:0 `Planar8` from a deterministic synthetic pattern.
    pub(in crate::vp8) fn pattern(width: u32, height: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let y = (0..w * h)
            .map(|i| ((i * 7 + i / w * 13) & 0xff) as u8)
            .collect();
        let u = (0..cw * ch).map(|i| ((i * 5 + 64) & 0xff) as u8).collect();
        let v = (0..cw * ch)
            .map(|i| ((i * 11 + 128) & 0xff) as u8)
            .collect();
        yuv420(width, height, y, u, v)
    }

    /// Four horizontal bands, one per quantizer segment, each lightly textured.
    ///
    /// Segments are assigned by macroblock luma mean (`(mean / 64).min(3)`), so a fixture only
    /// exercises the per-macroblock segment lookup if its macroblocks land in *different* buckets.
    /// `pattern`'s wrapping ramp does not: it sweeps the whole 0..255 range inside every
    /// macroblock, so each one averages near 128 and the segment map comes out uniform -- and a
    /// uniform map makes any index into it indistinguishable from any other.
    ///
    /// The bands average about 23, 87, 151 and 215, one per segment, and they are *horizontal* so
    /// the difference falls between macroblock rows -- what a wrong row stride gets wrong. All
    /// four matter: with only the outer two, the quantizer deltas for segments 1 and 2 are never
    /// applied and changing either goes unnoticed. The `i % 16` texture keeps blocks from being
    /// flat enough to skip entirely.
    ///
    /// `height` should cover four macroblock rows (64) so every band gets one.
    pub(in crate::vp8) fn banded(width: u32, height: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let y = (0..w * h)
            .map(|i| {
                let band = (i / w) * 4 / h.max(1);
                let base = [16usize, 80, 144, 208][band.min(3)];
                (base + (i % 16)) as u8
            })
            .collect();
        let u = (0..cw * ch).map(|i| ((i * 5 + 64) & 0xff) as u8).collect();
        let v = (0..cw * ch)
            .map(|i| ((i * 11 + 128) & 0xff) as u8)
            .collect();
        yuv420(width, height, y, u, v)
    }

    /// A coarse stepped gradient: structure enough for B_PRED, shallow enough to quantize away.
    ///
    /// Getting a macroblock that is *both* B_PRED and skipped needs two things that pull against
    /// each other -- structure a whole-block mode predicts badly, and a residual that vanishes at
    /// a coarse quantizer. `detailed` is too contrasty (its residual survives even at q=127) and
    /// flat content is too plain (a whole-block mode wins, so B_PRED is never chosen). Stepping
    /// the value every four pixels sits between the two.
    pub(in crate::vp8) fn stepped(width: u32, height: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let y = (0..w * h)
            .map(|i| {
                let (x, yy) = (i % w, i / w);
                (100 + (x / 4 + yy / 4) % 8 * 2) as u8
            })
            .collect();
        yuv420(width, height, y, vec![128; cw * ch], vec![128; cw * ch])
    }

    /// Builds B_PRED-favorable content: each 4×4 region carries a different gradient direction, so a
    /// single whole-block mode predicts the macroblock poorly but per-subblock modes do not.
    pub(in crate::vp8) fn detailed(width: u32, height: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let y = (0..w * h)
            .map(|i| {
                let (x, yy) = (i % w, i / w);
                let v = match (x / 4 + yy / 4) % 4 {
                    0 => x * 18,
                    1 => yy * 18,
                    2 => (x + yy) * 18,
                    _ => x.wrapping_sub(yy).wrapping_mul(18),
                };
                (v & 0xff) as u8
            })
            .collect();
        let u = (0..cw * ch).map(|i| ((i * 3) & 0xff) as u8).collect();
        let v = (0..cw * ch).map(|i| ((i * 9 + 70) & 0xff) as u8).collect();
        yuv420(width, height, y, u, v)
    }

    /// A structured ramp with real residuals in every plane, to exercise the transforms and tokens.
    pub(in crate::vp8) fn synthetic(width: u32, height: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let y = (0..w * h)
            .map(|i| ((i * 9 + (i / w) * 5) & 0xff) as u8)
            .collect();
        let u = (0..cw * ch).map(|i| ((i * 3 + 80) & 0xff) as u8).collect();
        let v = (0..cw * ch).map(|i| ((i * 7 + 150) & 0xff) as u8).collect();
        yuv420(width, height, y, u, v)
    }

    /// A frame with **photographic-like statistics** built directly in the YUV domain, so the
    /// bit-exact YUV conformance gate stays independent of the RGB↔YCbCr layer: a smooth luma
    /// gradient plus low-amplitude detail and hard region edges, with gently varying chroma.
    /// RNG-free and `seed`-parameterised, like `libwebp_oracle::fixtures::photo_like_rgba`.
    pub(in crate::vp8) fn photo_like(width: u32, height: u32, seed: u32) -> Planar8 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = chroma_dims(width, height);
        let hash = |x: i64, y: i64, k: i64| -> i64 {
            let mut v = x.wrapping_mul(374_761_393)
                ^ y.wrapping_mul(668_265_263)
                ^ k.wrapping_mul(2_654_435_761)
                ^ i64::from(seed).wrapping_mul(2_246_822_519);
            v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
            (v ^ (v >> 16)) & 0xff
        };
        let clamp = |v: i64| v.clamp(0, 255) as u8;
        let (wi, hi) = (w.max(1) as i64, h.max(1) as i64);
        let y = (0..w * h)
            .map(|i| {
                let (x, yy) = ((i % w) as i64, (i / w) as i64);
                // Smooth diagonal base + small detail + sharp 8-region steps (B_PRED / loop-filter
                // bait).
                let base = x * 170 / wi + yy * 60 / hi;
                let detail = (hash(x, yy, 0) - 128) / 10;
                let edge = if (x * 4 / wi + yy * 4 / hi) % 2 == 0 {
                    28
                } else {
                    0
                };
                clamp(base + detail + edge)
            })
            .collect();
        let (cwi, chi) = (cw.max(1) as i64, ch.max(1) as i64);
        let u = (0..cw * ch)
            .map(|i| {
                let (x, yy) = ((i % cw) as i64, (i / cw) as i64);
                clamp(100 + x * 70 / cwi + (hash(x, yy, 1) - 128) / 16)
            })
            .collect();
        let v = (0..cw * ch)
            .map(|i| {
                let (x, yy) = ((i % cw) as i64, (i / cw) as i64);
                clamp(140 + yy * 60 / chi + (hash(x, yy, 2) - 128) / 16)
            })
            .collect();
        yuv420(width, height, y, u, v)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{banded, chroma_dims, detailed, pattern, stepped, yuv420};
    use super::*;
    use crate::vp8::bool_coder::BoolDecoder;
    use crate::vp8::decoder::read_bmodes;
    use crate::vp8::header::{self, UNCOMPRESSED_CHUNK_LEN};
    use crate::vp8::prediction::{B_DC_PRED, B_PRED};

    /// Counts macroblocks coded as `B_PRED` by re-reading partition 0, to confirm the path is
    /// genuinely exercised (not merely available).
    /// Re-reads partition 0 (modes) and returns `(B_PRED macroblocks, skipped macroblocks)`, to
    /// confirm those paths are genuinely exercised.
    fn mode_stats(data: &[u8]) -> Modes {
        let chunk = header::read_uncompressed_chunk(data).unwrap();
        let part0_end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
        let mut modes = BoolDecoder::new(&data[UNCOMPRESSED_CHUNK_LEN..part0_end]);
        let (head, _) = header::read_frame_header(&chunk, &mut modes);
        let mb_cols = (chunk.width as usize).div_ceil(16);
        let mb_rows = (chunk.height as usize).div_ceil(16);
        let mut above_bmodes = vec![[B_DC_PRED; 4]; mb_cols];
        let (mut bpred, mut skipped, mut bpred_skipped) = (0, 0, 0);
        for _ in 0..mb_rows {
            let mut left_bmodes = [B_DC_PRED; 4];
            for mb_x in 0..mb_cols {
                if head.segmentation.update_map {
                    let _ = modes.get_tree(MB_SEGMENT_TREE, &head.segmentation.tree_probs);
                }
                let is_skipped = head.mb_no_skip_coeff && modes.get_bool(head.prob_skip_false);
                if is_skipped {
                    skipped += 1;
                }
                let y_mode = modes.get_tree(prediction::KF_YMODE_TREE, &prediction::KF_YMODE_PROB);
                let is_bpred = y_mode == B_PRED;
                let sub_modes = if is_bpred {
                    bpred += 1;
                    if is_skipped {
                        bpred_skipped += 1;
                    }
                    read_bmodes(&mut modes, &above_bmodes[mb_x], &left_bmodes)
                } else {
                    [B_DC_PRED; 16]
                };
                let _ = modes.get_tree(prediction::KF_UV_MODE_TREE, &prediction::KF_UV_MODE_PROB);
                (above_bmodes[mb_x], left_bmodes) = bmode_propagation(is_bpred, y_mode, &sub_modes);
            }
        }
        Modes {
            bpred,
            skipped,
            bpred_skipped,
        }
    }

    /// What `mode_stats` counts across a frame's macroblocks.
    struct Modes {
        bpred: usize,
        skipped: usize,
        /// Macroblocks that are *both* -- the pair `reconstruct_bpred_zero` exists for.
        bpred_skipped: usize,
    }

    /// A macroblock that is B_PRED *and* skipped, which is what `reconstruct_bpred_zero` is for.
    ///
    /// That function could be replaced with `()` outright and nothing failed (#110): B_PRED is
    /// covered, skipping is covered, but never the two together. Detailed content picks B_PRED,
    /// and a coarse quantizer makes its residual vanish -- so the encoder reconstructs those
    /// macroblocks from prediction alone, and if it does not, its recon stops matching the
    /// decoder's.
    ///
    /// The combination is asserted directly, not just the two counts separately, so this cannot
    /// quietly stop covering the pair if the encoder's mode decisions shift.
    #[test]
    fn a_skipped_bpred_macroblock_reconstructs_from_prediction_alone() {
        let yuv = stepped(48, 48);
        let (bits, recon) = encode_frame(&yuv, 110).expect("fixture fits the size fields");

        let modes = mode_stats(&bits);
        assert!(
            modes.bpred_skipped > 0,
            "need a macroblock that is both B_PRED and skipped, got bpred={} skipped={}",
            modes.bpred,
            modes.skipped
        );

        let decoded = decode_frame(&bits).expect("decode");
        let (enc, dec) = (recon.to_yuv420().unwrap(), decoded.to_yuv420().unwrap());
        assert_eq!(
            enc.planes()[0],
            dec.planes()[0],
            "luma mismatch on a skipped B_PRED frame"
        );
        assert_eq!(enc.planes()[1], dec.planes()[1], "u mismatch");
        assert_eq!(enc.planes()[2], dec.planes()[2], "v mismatch");
    }

    #[test]
    fn bpred_is_exercised_and_bit_exact() {
        let yuv = detailed(48, 48);
        let (bitstream, recon) =
            encode_frame(&yuv, 8).expect("fixture fits the partition-size fields");
        assert!(
            mode_stats(&bitstream).bpred > 0,
            "detailed content should select B_PRED for some macroblocks"
        );
        let decoded = decode_frame(&bitstream).expect("decode");
        let (enc, dec) = (recon.to_yuv420().unwrap(), decoded.to_yuv420().unwrap());
        assert_eq!(enc.planes()[0], dec.planes()[0], "B_PRED luma mismatch");
        assert_eq!(enc.planes()[1], dec.planes()[1], "B_PRED u mismatch");
        assert_eq!(enc.planes()[2], dec.planes()[2], "B_PRED v mismatch");
    }

    #[test]
    fn mb_skip_is_exercised_and_bit_exact() {
        // A flat image predicts to 128 with a zero residual, so every macroblock is skipped; the
        // decode must reproduce it from the skip flags alone.
        let (w, h) = (48u32, 48u32);
        let (cw, ch) = chroma_dims(w, h);
        let yuv = yuv420(
            w,
            h,
            vec![128u8; (w * h) as usize],
            vec![128u8; cw * ch],
            vec![128u8; cw * ch],
        );
        let (bits, recon) = encode_frame(&yuv, 60).expect("fixture fits the partition-size fields");
        assert!(
            mode_stats(&bits).skipped > 0,
            "flat content should skip macroblocks"
        );
        let dec = decode_frame(&bits).expect("decode");
        assert_eq!(
            recon.to_yuv420().unwrap().planes()[0],
            dec.to_yuv420().unwrap().planes()[0]
        );
        assert_eq!(
            recon.to_yuv420().unwrap().planes()[1],
            dec.to_yuv420().unwrap().planes()[1]
        );
        assert_eq!(
            recon.to_yuv420().unwrap().planes()[2],
            dec.to_yuv420().unwrap().planes()[2]
        );
    }

    /// Tier-2: the encoder's reconstruction must equal the native decoder's output, bit-for-bit.
    fn assert_encoder_recon_matches_decoder(width: u32, height: u32, q: u8) {
        let yuv = pattern(width, height);
        let (bitstream, recon) =
            encode_frame(&yuv, q).expect("fixture fits the partition-size fields");
        let decoded = decode_frame(&bitstream).expect("decode");
        let enc = recon.to_yuv420().unwrap();
        let dec = decoded.to_yuv420().unwrap();
        assert_eq!(
            enc.planes()[0],
            dec.planes()[0],
            "luma mismatch at {width}x{height} q{q}"
        );
        assert_eq!(
            enc.planes()[1],
            dec.planes()[1],
            "u mismatch at {width}x{height} q{q}"
        );
        assert_eq!(
            enc.planes()[2],
            dec.planes()[2],
            "v mismatch at {width}x{height} q{q}"
        );
    }

    #[test]
    fn encoder_recon_matches_decoder_across_sizes_and_quant() {
        for &(w, h) in &[
            (16u32, 16u32),
            (32, 16),
            (17, 9),
            (1, 1),
            (64, 48),
            (33, 41),
        ] {
            for &q in &[0u8, 10, 40, 80, 127] {
                assert_encoder_recon_matches_decoder(w, h, q);
            }
        }
    }

    /// The recon-equals-decoder invariant with quantizer segmentation on.
    ///
    /// `assert_encoder_recon_matches_decoder` above runs through `encode_frame`, so it always
    /// encodes with segmentation off and every macroblock on the frame quantizer. This runs the
    /// same invariant with it on, over content whose macroblocks land in different segments.
    ///
    /// It does *not* catch a wrong per-macroblock segment lookup -- see
    /// `segmented_stream_is_byte_stable` below for why nothing consistency-shaped can.
    #[test]
    fn encoder_recon_matches_decoder_with_segmentation() {
        let opts = EncodeOptions {
            segmented: true,
            ..EncodeOptions::default()
        };
        // Sizes spanning more than one macroblock row, so `mb_y` is not always 0 -- at one row the
        // row-stride arithmetic cannot be distinguished from anything.
        for &(w, h) in &[(64u32, 48u32), (33, 41)] {
            for &q in &[10u8, 60] {
                let yuv = banded(w, h);
                let (bits, recon) =
                    encode_frame_filtered(&yuv, q, opts).expect("fixture fits the size fields");
                let decoded = decode_frame(&bits).expect("decode");
                let (enc, dec) = (recon.to_yuv420().unwrap(), decoded.to_yuv420().unwrap());
                assert_eq!(
                    enc.planes()[0],
                    dec.planes()[0],
                    "luma mismatch at {w}x{h} q{q} segmented"
                );
                assert_eq!(
                    enc.planes()[1],
                    dec.planes()[1],
                    "u mismatch at {w}x{h} q{q} segmented"
                );
                assert_eq!(
                    enc.planes()[2],
                    dec.planes()[2],
                    "v mismatch at {w}x{h} q{q} segmented"
                );
            }
        }
    }

    #[test]
    fn both_loop_filters_reconstruct_bit_exact() {
        // The simple (luma-only) and normal (luma+chroma) filters must each reconstruct identically
        // in the encoder and decoder — exercising both decoder filter paths on coefficient-bearing
        // content (so interior edges are filtered too).
        for simple in [true, false] {
            for &q in &[20u8, 60, 110] {
                let yuv = detailed(48, 32);
                let opts = EncodeOptions {
                    simple_filter: simple,
                    segmented: false,
                    partitions: 1,
                    ..Default::default()
                };
                let (bits, recon) = encode_frame_filtered(&yuv, q, opts)
                    .expect("fixture fits the partition-size fields");
                let dec = decode_frame(&bits).expect("decode");
                let (enc, dec) = (recon.to_yuv420().unwrap(), dec.to_yuv420().unwrap());
                assert_eq!(
                    enc.planes()[0],
                    dec.planes()[0],
                    "luma simple={simple} q{q}"
                );
                assert_eq!(enc.planes()[1], dec.planes()[1], "u simple={simple} q{q}");
                assert_eq!(enc.planes()[2], dec.planes()[2], "v simple={simple} q{q}");
            }
        }
    }

    #[test]
    fn segmentation_round_trips_bit_exact() {
        // Four quantizer segments (assigned by macroblock luma mean) must reconstruct identically in
        // the encoder and the decoder across a range of base quantizers.
        for &q in &[10u8, 40, 90] {
            let yuv = detailed(64, 48);
            let opts = EncodeOptions {
                simple_filter: false,
                segmented: true,
                partitions: 1,
                ..Default::default()
            };
            let (bits, recon) = encode_frame_filtered(&yuv, q, opts)
                .expect("fixture fits the partition-size fields");
            let dec = decode_frame(&bits).expect("decode");
            let (enc, dec) = (recon.to_yuv420().unwrap(), dec.to_yuv420().unwrap());
            assert_eq!(enc.planes()[0], dec.planes()[0], "luma q{q}");
            assert_eq!(enc.planes()[1], dec.planes()[1], "u q{q}");
            assert_eq!(enc.planes()[2], dec.planes()[2], "v q{q}");
        }
    }

    #[test]
    fn token_partitions_round_trip_bit_exact() {
        // 1/2/4/8 token partitions must each reconstruct identically; a tall image routes macroblock
        // rows across all eight partitions.
        for partitions in [1u8, 2, 4, 8] {
            let yuv = detailed(32, 160);
            let opts = EncodeOptions {
                simple_filter: false,
                segmented: false,
                partitions,
                ..Default::default()
            };
            let (bits, recon) = encode_frame_filtered(&yuv, 30, opts)
                .expect("fixture fits the partition-size fields");
            let dec = decode_frame(&bits).expect("decode");
            let (enc, dec) = (recon.to_yuv420().unwrap(), dec.to_yuv420().unwrap());
            assert_eq!(enc.planes()[0], dec.planes()[0], "luma p{partitions}");
            assert_eq!(enc.planes()[1], dec.planes()[1], "u p{partitions}");
            assert_eq!(enc.planes()[2], dec.planes()[2], "v p{partitions}");
        }
    }

    #[test]
    fn loop_filter_deltas_reconstruct_bit_exact() {
        // mb_lf_adjustments shift each macroblock's filter level; the encoder writes them into the
        // header and applies them to its own reconstruction, so the native decoder must reproduce the
        // same deblocked planes. detailed() forces B_PRED macroblocks, so the mode[0] (B_PRED) delta
        // is genuinely exercised, not just ref_frame[0]. (The cross-check that this matches libwebp's
        // own delta handling is the tier-3 oracle in `oracle_tests` below.)
        for ref_d in [-16i8, 0, 12] {
            for mode_d in [-8i8, 0, 10] {
                for &q in &[20u8, 60] {
                    let yuv = detailed(48, 32);
                    let opts = EncodeOptions {
                        loop_filter_deltas: LoopFilterDeltas {
                            ref_frame: [ref_d, 0, 0, 0],
                            mode: [mode_d, 0, 0, 0],
                        },
                        ..Default::default()
                    };
                    let (bits, recon) = encode_frame_filtered(&yuv, q, opts)
                        .expect("fixture fits the partition-size fields");
                    let dec = decode_frame(&bits).expect("decode");
                    let (enc, dec) = (recon.to_yuv420().unwrap(), dec.to_yuv420().unwrap());
                    assert_eq!(
                        enc.planes()[0],
                        dec.planes()[0],
                        "luma ref={ref_d} mode={mode_d} q{q}"
                    );
                    assert_eq!(
                        enc.planes()[1],
                        dec.planes()[1],
                        "u ref={ref_d} mode={mode_d} q{q}"
                    );
                    assert_eq!(
                        enc.planes()[2],
                        dec.planes()[2],
                        "v ref={ref_d} mode={mode_d} q{q}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod oracle_tests;

#[cfg(test)]
mod robustness_tests;
