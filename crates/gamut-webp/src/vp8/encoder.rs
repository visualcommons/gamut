//! VP8 key-frame encoder (RFC 6386 §9-§14 read in the writing direction): the per-macroblock mode
//! decision (lowest-SAD whole-block 16×16 luma / 8×8 chroma, or per-4×4 `B_PRED` behind the effort
//! rung's gate), quantizer segmentation by macroblock luma mean, forward transform and quantization,
//! the measured skip and coefficient probabilities, and the two-pass write of the control and token
//! partitions.
//!
//! Every decision is reconstructed through [`super::reconstruct`] exactly as the decoder will, so the
//! [`FrameBuffers`] this returns is bit-identical to any conformant decoder's output.

// The macroblock/block math indexes several fixed-size arrays in lock-step (and over partial ranges
// like `1..16`), where explicit indices read closer to the spec than iterator adaptors.
#![allow(clippy::needless_range_loop)]

use gamut_color::{ChromaSubsampling, Planar8};
use gamut_core::{Error, Result};

use super::bool_coder::BoolEncoder;
use super::cost::bit_cost;
use super::effort::{Bpred, EFFORT_TABLE, QuantBias};
use super::frame::{
    EntropyCtx, MB_SEGMENT_TREE, MbLevels, bmode_context, bmode_propagation, clear_mb_context,
    mb_has_coeffs,
};
use super::header::{
    self, LoopFilterDeltas, LoopFilterParams, QuantIndices, Segmentation, Vp8FrameHeader,
};
use super::prediction::{self, B_DC_PRED, B_PRED, DC_PRED, H_PRED, NUM_BMODES, TM_PRED, V_PRED};
use super::quant::{self, QuantFactors};
use super::reconstruct::{
    FrameBuffers, above_right_source, apply_loop_filter, predict_chroma, predict_luma,
    reconstruct_chroma, reconstruct_luma, segment_quant_factors, sub_pred, subblock_neighbors,
    write_block,
};
use super::tokens::{self, CoeffProbs};
use super::transform::{fdct4x4, fwht4x4, idct4x4};
use crate::config::Effort;

/// The whole-block prediction modes the encoder considers, in signaling order.
const WHOLE_BLOCK_MODES: [usize; 4] = [DC_PRED, V_PRED, H_PRED, TM_PRED];

/// SAD margin by which per-subblock `B_PRED` must beat the best whole-block mode to be chosen — a
/// coarse stand-in for `B_PRED`'s extra mode-signaling cost (true rate-distortion search is issue #32).
const BPRED_SAD_PENALTY: u32 = 160;

/// Mean absolute prediction error per luma pixel above which the gated rungs will consider
/// `B_PRED`. Below it the whole-block modes already fit, and the 4x4 search would not repay its
/// cost.
const BPRED_GATE_SAD_PER_PIXEL: u32 = 6;

/// The gated rungs' `B_PRED` admission test: a macroblock is worth a 4x4 search only when the
/// whole-block prediction misses by more than [`BPRED_GATE_SAD_PER_PIXEL`] per luma pixel
/// (256 of them). Strict, so a macroblock exactly at the threshold is *not* admitted.
///
/// Split out from the mode loop because the threshold is where the rung differs from its
/// neighbours, and an inequality is only pinned by the value that sits on it.
fn bpred_gate_admits(wb_sad: u32) -> bool {
    wb_sad > BPRED_GATE_SAD_PER_PIXEL * 256
}

/// Whether the 4x4 prediction wins the macroblock: its SAD plus [`BPRED_SAD_PENALTY`] — a coarse
/// stand-in for the extra mode-signaling `B_PRED` costs — must come in strictly under the whole
/// block's. A tie keeps the whole-block mode, which is the cheaper one to signal.
fn bpred_beats_whole_block(bpred_sad: u32, wb_sad: u32) -> bool {
    bpred_sad + BPRED_SAD_PENALTY < wb_sad
}

/// The largest non-final token partition the bitstream can describe: its size is a 3-byte
/// little-endian prefix (RFC 6386 §9.5), so 16 MiB - 1. The final partition carries no prefix —
/// its length is the remainder — so it is unbounded.
pub const MAX_TOKEN_PARTITION_SIZE: u32 = (1 << 24) - 1;

/// Per-segment quantizer deltas the encoder assigns (delta mode) when segmentation is enabled — a
/// coarse spread so distinct macroblock regions get distinct quantizers (refinement is issue #32).
const SEGMENT_QUANT_DELTAS: [i8; 4] = [-12, -4, 4, 12];

/// Encoder feature toggles for a frame. Defaults to the normal loop filter, no segmentation, a
/// single token partition, and no per-macroblock loop-filter deltas.
#[derive(Clone, Copy)]
pub struct EncodeOptions {
    /// Use the simple loop filter instead of the normal one.
    pub simple_filter: bool,
    /// Emit four quantizer segments, assigned per macroblock by luma mean.
    pub segmented: bool,
    /// Number of DCT token partitions (1, 2, 4, or 8); macroblock rows are assigned round-robin.
    pub partitions: u8,
    /// Per-macroblock loop-filter deltas (`mb_lf_adjustments`, RFC 6386 §9.4). For key frames the
    /// intra `ref_frame[0]` delta shifts every macroblock's filter level and the `B_PRED` `mode[0]`
    /// delta shifts 4×4-predicted ones; the default (all-zero) emits the disabled record.
    pub loop_filter_deltas: LoopFilterDeltas,
    /// Compression effort: which coding tools the encoder may spend time on. Every level emits a
    /// conformant key frame, so this trades encode time for size, never correctness.
    pub effort: Effort,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            simple_filter: false,
            segmented: false,
            partitions: 1,
            loop_filter_deltas: LoopFilterDeltas::default(),
            effort: Effort::Default,
        }
    }
}

/// The mean luma of macroblock `(mb_x, mb_y)` in a `stride`-wide plane, used to assign its segment.
fn mb_luma_mean(src: &[u8], stride: usize, mb_x: usize, mb_y: usize) -> u32 {
    let (px, py) = (mb_x * 16, mb_y * 16);
    let mut sum = 0u32;
    for r in 0..16 {
        for c in 0..16 {
            sum += u32::from(src[(py + r) * stride + px + c]);
        }
    }
    sum / 256
}

/// One macroblock's coding decisions — everything the writing pass needs to emit its mode and token
/// bits.
///
/// Reconstruction has already happened by the time this exists, and nothing in it depends on a
/// probability: changing the frame's probabilities changes only the bit cost, never a decoded pixel.
/// That is what makes measuring the probabilities between the two passes exact rather than an
/// approximation.
#[derive(Clone)]
struct MbRecord {
    /// The quantized coefficient levels for every block of the macroblock.
    levels: MbLevels,
    /// The 16 `B_PRED` submodes; meaningful only when `y_mode == B_PRED`.
    sub_modes: [usize; 16],
    /// The coded luma mode.
    y_mode: usize,
    /// The whole-block luma mode that was considered, which the `B_PRED` context propagation needs
    /// even when `B_PRED` won.
    wb_mode: usize,
    /// The coded chroma mode.
    uv_mode: usize,
    /// The quantizer segment this macroblock was assigned.
    segment: usize,
    /// Whether every block came out all-zero, so no tokens are coded.
    skip: bool,
}

/// The probability that a macroblock is **not** skipped, measured from what the frame actually
/// produced (RFC 6386 §9.10).
///
/// The single-pass encoder had to guess this from the quantizer. Measuring it costs nothing once
/// the decisions are recorded, and it is coded once in the header against one bool per macroblock.
fn measured_skip_prob(records: &[MbRecord]) -> u8 {
    let total = records.len() as u32;
    if total == 0 {
        return 1;
    }
    let skipped = records.iter().filter(|r| r.skip).count() as u32;
    // `put_bool(prob_skip_false, skip)` codes `skip` as the *one* branch, so the stored probability
    // is that of the zero branch — not skipping.
    ((255 * (total - skipped)) / total).clamp(1, 255) as u8
}

/// Tallies the zero/one branches every coefficient token in the frame would take, threading the
/// same above/left non-zero contexts the writing pass will.
fn tally_coeff_bits(records: &[MbRecord], mb_cols: usize) -> tokens::CoeffCounts {
    let mut counts: tokens::CoeffCounts =
        [[[[[0; 2]; tokens::ENTROPY_NODES]; 3]; tokens::COEFF_BANDS]; tokens::PLANE_TYPES];
    let mut above = vec![EntropyCtx::default(); mb_cols];
    for chunk in records.chunks(mb_cols) {
        let mut left = EntropyCtx::default();
        for (mb_x, record) in chunk.iter().enumerate() {
            let use_bpred = record.y_mode == B_PRED;
            if record.skip {
                clear_mb_context(&mut above[mb_x], &mut left, use_bpred);
            } else {
                count_mb_tokens(
                    &mut counts,
                    &mut above[mb_x],
                    &mut left,
                    &record.levels,
                    use_bpred,
                );
            }
        }
    }
    counts
}

/// Derives the frame's coefficient probabilities from measured token counts, adopting a measured
/// value only where it pays for its own update record (RFC 6386 §13.4).
///
/// A context that was never exercised keeps its default: there is nothing to learn from it, and
/// coding an update for it would be pure loss.
fn optimize_coeff_probs(counts: &tokens::CoeffCounts) -> tokens::CoeffProbs {
    let mut probs = tokens::DEFAULT_COEFF_PROBS;
    for plane in 0..tokens::PLANE_TYPES {
        for band in 0..tokens::COEFF_BANDS {
            for ctx in 0..3 {
                for node in 0..tokens::ENTROPY_NODES {
                    // Widened to `u64` before anything is multiplied: these are frame-wide
                    // tallies, and a single hot context on a large frame holds millions of
                    // events. At up to 2048 cost units each (`bit_cost`'s maximum), the product
                    // leaves `u32` around two million events — well inside the canvas sizes
                    // WebP allows, so `u32` here is an overflow, not a bound.
                    let [zeros, ones] = counts[plane][band][ctx][node].map(u64::from);
                    let total = zeros + ones;
                    if total == 0 {
                        continue;
                    }
                    let old = tokens::DEFAULT_COEFF_PROBS[plane][band][ctx][node];
                    let new = ((zeros * 255) / total).clamp(1, 255) as u8;
                    if new == old {
                        continue;
                    }
                    let update_prob = tokens::COEFF_UPDATE_PROBS[plane][band][ctx][node];
                    let old_cost = zeros * u64::from(bit_cost(false, old))
                        + ones * u64::from(bit_cost(true, old))
                        + u64::from(bit_cost(false, update_prob));
                    // Adopting costs the "yes, update" flag plus the eight literal bits of the new
                    // value, on top of coding every token at the new probability.
                    let new_cost = zeros * u64::from(bit_cost(false, new))
                        + ones * u64::from(bit_cost(true, new))
                        + u64::from(bit_cost(true, update_prob))
                        + 8 * 256;
                    if new_cost < old_cost {
                        probs[plane][band][ctx][node] = new;
                    }
                }
            }
        }
    }
    probs
}

/// Picks a loop-filter strength from the base quantizer — stronger quantization deblocks harder. A
/// coarse heuristic (true filter-level selection is part of issue #32); a level of 0 disables it.
fn filter_level(quant_index: u8) -> u8 {
    quant_index / 2
}

/// Builds the minimal key-frame header for the given dimensions, base quantizer, and filter type.
fn frame_header(width: u32, height: u32, quant_index: u8, simple_filter: bool) -> Vp8FrameHeader {
    Vp8FrameHeader {
        width: width as u16,
        height: height as u16,
        horizontal_scale: 0,
        vertical_scale: 0,
        version: 0,
        color_space: 0,
        clamp_required: true,
        segmentation: Segmentation::default(),
        loop_filter: LoopFilterParams {
            simple: simple_filter,
            level: filter_level(quant_index),
            sharpness: 0,
            deltas: LoopFilterDeltas::default(),
        },
        token_partitions: 1,
        quant: QuantIndices {
            y_ac: quant_index,
            ..QuantIndices::default()
        },
        refresh_entropy_probs: true,
        // Enable per-macroblock skip coding. The skip-false probability falls with the quantizer,
        // since coarser quantization yields more all-zero (skippable) macroblocks.
        mb_no_skip_coeff: true,
        prob_skip_false: (255 - quant_index).max(1),
    }
}

/// Replicates `src` (`sw × sh`) into a `dw × dh` plane, extending the right and bottom edges.
fn pad_plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dw * dh];
    for y in 0..dh {
        let sy = y.min(sh - 1);
        for x in 0..dw {
            dst[y * dw + x] = src[sy * sw + x.min(sw - 1)];
        }
    }
    dst
}

/// Reads a 4×4 block at `(x, y)` of `plane` as 16-bit samples.
fn read_block(plane: &[u8], stride: usize, x: usize, y: usize) -> [i16; 16] {
    let mut b = [0i16; 16];
    for r in 0..4 {
        for c in 0..4 {
            b[r * 4 + c] = i16::from(plane[(y + r) * stride + x + c]);
        }
    }
    b
}

/// Sum of absolute differences between an `n`×`n` prediction and the source macroblock.
fn block_sad(pred: &[u8], src: &[u8], stride: usize, mb_x: usize, mb_y: usize, n: usize) -> u32 {
    let mut sad = 0u32;
    for r in 0..n {
        for c in 0..n {
            let s = i32::from(src[(mb_y * n + r) * stride + mb_x * n + c]);
            sad += s.abs_diff(i32::from(pred[r * n + c]));
        }
    }
    sad
}

/// Selects the lowest-SAD whole-block luma mode (a simple proxy; rate-distortion search is issue #32).
fn select_luma_mode(
    recon: &FrameBuffers,
    src: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
) -> usize {
    let mut best = (DC_PRED, u32::MAX);
    for mode in WHOLE_BLOCK_MODES {
        let sad = block_sad(
            &predict_luma(recon, mb_x, mb_y, mode),
            src,
            stride,
            mb_x,
            mb_y,
            16,
        );
        if sad < best.1 {
            best = (mode, sad);
        }
    }
    best.0
}

/// Selects the lowest-combined-SAD chroma mode (shared by U and V).
fn select_chroma_mode(
    recon: &FrameBuffers,
    src_u: &[u8],
    src_v: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
) -> usize {
    let mut best = (DC_PRED, u32::MAX);
    for mode in WHOLE_BLOCK_MODES {
        let su = block_sad(
            &predict_chroma(&recon.u, recon.c_stride(), mb_x, mb_y, mode),
            src_u,
            stride,
            mb_x,
            mb_y,
            8,
        );
        let sv = block_sad(
            &predict_chroma(&recon.v, recon.c_stride(), mb_x, mb_y, mode),
            src_v,
            stride,
            mb_x,
            mb_y,
            8,
        );
        if su + sv < best.1 {
            best = (mode, su + sv);
        }
    }
    best.0
}

/// Forward-quantizes one coefficient under the effort level's rounding rule.
fn quantize_with(bias: QuantBias, coeff: i16, factor: i16, dead_zone: u16) -> i16 {
    match bias {
        QuantBias::Nearest => quant::quantize(coeff, factor),
        QuantBias::DeadZone => quant::quantize_biased(coeff, factor, dead_zone),
    }
}

/// Transforms + quantizes one luma macroblock against its prediction, returning the Y2 and per
/// sub-block AC levels.
#[allow(clippy::too_many_arguments)] // source, position, prediction, quantizer, and output
fn quantize_luma(
    src: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 256],
    qf: &QuantFactors,
    bias: QuantBias,
    levels: &mut MbLevels,
) {
    let mut y_coeffs = [[0i16; 16]; 16];
    let mut y_dc = [0i16; 16];
    for i in 0..16 {
        let (sc, sr) = (i % 4, i / 4);
        let block = read_block(src, stride, mb_x * 16 + sc * 4, mb_y * 16 + sr * 4);
        let p = sub_pred(pred, 16, sc * 4, sr * 4);
        let residue: [i16; 16] = core::array::from_fn(|k| block[k] - p[k]);
        y_coeffs[i] = fdct4x4(&residue);
        y_dc[i] = y_coeffs[i][0];
    }
    let y2_coeffs = fwht4x4(&y_dc);
    levels.y2[0] = quantize_with(bias, y2_coeffs[0], qf.y2_dc, quant::BIAS_DC);
    for k in 1..16 {
        levels.y2[k] = quantize_with(bias, y2_coeffs[k], qf.y2_ac, quant::BIAS_AC);
    }
    for i in 0..16 {
        for k in 1..16 {
            levels.y[i][k] = quantize_with(bias, y_coeffs[i][k], qf.y1_ac, quant::BIAS_AC);
        }
    }
}

/// Transforms + quantizes one chroma plane's four sub-blocks against its prediction.
fn quantize_chroma(
    src: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 64],
    qf: &QuantFactors,
    bias: QuantBias,
) -> [[i16; 16]; 4] {
    let mut levels = [[0i16; 16]; 4];
    for i in 0..4 {
        let (sc, sr) = (i % 2, i / 2);
        let block = read_block(src, stride, mb_x * 8 + sc * 4, mb_y * 8 + sr * 4);
        let p = sub_pred(pred, 8, sc * 4, sr * 4);
        let residue: [i16; 16] = core::array::from_fn(|k| block[k] - p[k]);
        let coeffs = fdct4x4(&residue);
        levels[i][0] = quantize_with(bias, coeffs[0], qf.uv_dc, quant::BIAS_DC);
        for k in 1..16 {
            levels[i][k] = quantize_with(bias, coeffs[k], qf.uv_ac, quant::BIAS_AC);
        }
    }
    levels
}

/// Encodes the luma plane of a `B_PRED` macroblock: per subblock (raster order), selects the
/// lowest-SAD submode, quantizes the residual (plane 3 — DC included, no Y2), and reconstructs in
/// place so the next subblock predicts from it. Returns the 16 submodes, their quantized levels, and
/// the total prediction SAD (for the macroblock mode decision).
#[allow(clippy::too_many_arguments)] // the per-subblock search genuinely needs all of this state
fn encode_bpred_luma(
    recon: &mut FrameBuffers,
    src: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    qf: &QuantFactors,
    bias: QuantBias,
    above_right: &[u8; 4],
) -> ([usize; 16], [[i16; 16]; 16], u32) {
    let (px, py, rstride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    let mut sub_modes = [B_DC_PRED; 16];
    let mut levels = [[0i16; 16]; 16];
    let mut total_sad = 0u32;
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let (sx, sy) = (px + c * 4, py + r * 4);
        let (a, l, corner) = subblock_neighbors(recon, sx, sy, c, above_right);
        let src_sub = read_block(src, stride, sx, sy);
        let mut best = (B_DC_PRED, u32::MAX, [0u8; 16]);
        for m in 0..NUM_BMODES {
            let pred = prediction::subblock_predict(m, &a, &l, corner);
            let sad: u32 = (0..16)
                .map(|k| i32::from(src_sub[k]).abs_diff(i32::from(pred[k])))
                .sum();
            if sad < best.1 {
                best = (m, sad, pred);
            }
        }
        let (mode, sad, pred) = best;
        sub_modes[i] = mode;
        total_sad += sad;

        let residue: [i16; 16] = core::array::from_fn(|k| src_sub[k] - i16::from(pred[k]));
        let coeffs = fdct4x4(&residue);
        levels[i][0] = quantize_with(bias, coeffs[0], qf.y1_dc, quant::BIAS_DC);
        for k in 1..16 {
            levels[i][k] = quantize_with(bias, coeffs[k], qf.y1_ac, quant::BIAS_AC);
        }
        let mut dq = [0i16; 16];
        dq[0] = quant::dequantize(levels[i][0], qf.y1_dc);
        for k in 1..16 {
            dq[k] = quant::dequantize(levels[i][k], qf.y1_ac);
        }
        let residue = idct4x4(&dq);
        let pred_i16: [i16; 16] = core::array::from_fn(|k| i16::from(pred[k]));
        write_block(&mut recon.y, rstride, sx, sy, &pred_i16, &residue);
    }
    (sub_modes, levels, total_sad)
}

/// Writes the 16 `B_PRED` submodes, each tree-coded with its neighbor context (RFC 6386 §11.3).
fn write_bmodes(
    modes: &mut BoolEncoder,
    sub_modes: &[usize; 16],
    above_col: &[usize; 4],
    left_col: &[usize; 4],
) {
    for i in 0..16 {
        let (a, l) = bmode_context(sub_modes, above_col, left_col, i);
        modes.put_tree(
            prediction::BMODE_TREE,
            &prediction::KF_BMODE_PROB[a][l],
            sub_modes[i],
        );
    }
}

/// Codes one macroblock's coefficient blocks in Y2 → Y → U → V order, threading the `above`/`left`
/// non-zero context (RFC 6386 §13.3). A `B_PRED` macroblock has no Y2 block (its context persists)
/// and codes luma with plane 3 (DC included); otherwise luma uses plane 0 (DC carried by Y2).
fn encode_mb_tokens(
    enc: &mut BoolEncoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    levels: &MbLevels,
    is_bpred: bool,
) {
    if !is_bpred {
        let ctx = usize::from(above.y2) + usize::from(left.y2);
        let has = tokens::encode_block(enc, &levels.y2, 1, ctx, probs);
        above.y2 = has;
        left.y2 = has;
    }
    let plane = if is_bpred { 3 } else { 0 };
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let has = tokens::encode_block(enc, &levels.y[i], plane, ctx, probs);
        above.y[c] = has;
        left.y[r] = has;
    }
    encode_chroma_tokens(enc, above, left, probs, levels);
}

/// Tallies the branches a macroblock's coefficient tokens would take, threading the same above/left
/// contexts [`encode_mb_tokens`] does.
///
/// Deliberately mirrors that function's *shape* while sharing its per-block tokenization through
/// [`tokens::count_block`], so the two cannot disagree about which bits exist.
fn count_mb_tokens(
    counts: &mut tokens::CoeffCounts,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    levels: &MbLevels,
    is_bpred: bool,
) {
    if !is_bpred {
        let ctx = usize::from(above.y2) + usize::from(left.y2);
        let has = tokens::count_block(counts, &levels.y2, 1, ctx);
        above.y2 = has;
        left.y2 = has;
    }
    let plane = if is_bpred { 3 } else { 0 };
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let has = tokens::count_block(counts, &levels.y[i], plane, ctx);
        above.y[c] = has;
        left.y[r] = has;
    }
    for (plane_levels, above_ctx, left_ctx) in [
        (&levels.u, &mut above.u, &mut left.u),
        (&levels.v, &mut above.v, &mut left.v),
    ] {
        for i in 0..4 {
            let (r, c) = (i / 2, i % 2);
            let ctx = usize::from(above_ctx[c]) + usize::from(left_ctx[r]);
            let has = tokens::count_block(counts, &plane_levels[i], 2, ctx);
            above_ctx[c] = has;
            left_ctx[r] = has;
        }
    }
}

/// Codes a macroblock's U then V chroma blocks (plane 2).
fn encode_chroma_tokens(
    enc: &mut BoolEncoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    levels: &MbLevels,
) {
    for (plane_levels, above_ctx, left_ctx) in [
        (&levels.u, &mut above.u, &mut left.u),
        (&levels.v, &mut above.v, &mut left.v),
    ] {
        for i in 0..4 {
            let (r, c) = (i / 2, i % 2);
            let ctx = usize::from(above_ctx[c]) + usize::from(left_ctx[r]);
            let has = tokens::encode_block(enc, &plane_levels[i], 2, ctx, probs);
            above_ctx[c] = has;
            left_ctx[r] = has;
        }
    }
}

/// Encodes 4:2:0 [`Planar8`] planes as a VP8 key-frame bitstream (the `VP8 ` chunk payload), returning the
/// bitstream and the encoder's reconstruction (the tier-2 oracle: it must equal any decoder's output).
/// Uses the normal loop filter. Test-only: the crate's own encoder always passes explicit
/// [`EncodeOptions`] to [`encode_frame_filtered`].
///
/// # Errors
///
/// As [`encode_frame_filtered`].
#[cfg(test)]
pub fn encode_frame(yuv: &Planar8, quant_index: u8) -> Result<(Vec<u8>, FrameBuffers)> {
    encode_frame_filtered(yuv, quant_index, EncodeOptions::default())
}

/// Encodes a frame with explicit [`EncodeOptions`] — the loop-filter type and whether to emit
/// quantizer segments. `encode_frame` uses the defaults (normal filter, unsegmented). This lets the
/// differential oracle drive the alternative encoder paths.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when the coded
/// frame outgrows one of the bitstream's own partition-size fields: the control partition past
/// `MAX_FIRST_PARTITION_SIZE` (19 bits, RFC 6386 §9.1),
/// or a non-final token partition past [`MAX_TOKEN_PARTITION_SIZE`] (24 bits, §9.5). Both are
/// format ceilings a large, highly detailed frame can genuinely reach — the mode records of a
/// `B_PRED`-heavy frame are what fill the control partition — and neither can be encoded, so the
/// alternative to reporting them is emitting a stream no decoder accepts. libwebp reports the same
/// condition (`VP8_ENC_ERROR_PARTITION0_OVERFLOW`) rather than degrading quality behind the
/// caller's back, and neither does this. The lever is [`EncodeOptions::effort`]: `B_PRED` is what
/// fills partition 0, so [`Effort::Fastest`] (which never emits it) encodes the whole WebP canvas
/// range, up to 16383x16383, at any detail level.
///
/// Also returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when
/// `yuv` is not laid out [`ChromaSubsampling::Cs420`], the only layout VP8 codes (RFC 6386 §9.2).
pub fn encode_frame_filtered(
    yuv: &Planar8,
    quant_index: u8,
    opts: EncodeOptions,
) -> Result<(Vec<u8>, FrameBuffers)> {
    if yuv.subsampling() != ChromaSubsampling::Cs420 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: only 4:2:0 planes are coded",
        ));
    }
    let mut header = frame_header(yuv.width(), yuv.height(), quant_index, opts.simple_filter);
    if opts.segmented {
        header.segmentation = Segmentation {
            enabled: true,
            update_map: true,
            abs_delta: false,
            quantizer: SEGMENT_QUANT_DELTAS,
            filter_strength: [0; 4],
            tree_probs: [128, 128, 128],
        };
    }
    let tools = EFFORT_TABLE[opts.effort.level() as usize];
    header.token_partitions = opts.partitions.max(1);
    header.loop_filter.deltas = opts.loop_filter_deltas;
    let n = header.token_partitions as usize;
    let seg_qf = segment_quant_factors(&header);
    let mut recon = FrameBuffers::new(yuv.width(), yuv.height());

    let (yw, yh) = (recon.y_stride(), recon.mb_rows * 16);
    let (cw, ch) = (recon.c_stride(), recon.mb_rows * 8);
    let [y_plane, u_plane, v_plane] = yuv.planes();
    let src_y = pad_plane(y_plane, yuv.width() as usize, yuv.height() as usize, yw, yh);
    let (vcw, vch) = ChromaSubsampling::Cs420.chroma_dimensions(yuv.width(), yuv.height());
    let (vcw, vch) = (vcw as usize, vch as usize);
    let src_u = pad_plane(u_plane, vcw, vch, cw, ch);
    let src_v = pad_plane(v_plane, vcw, vch, cw, ch);

    let segment_map: Vec<usize> = (0..recon.mb_rows * recon.mb_cols)
        .map(|i| {
            if header.segmentation.enabled {
                let (mbx, mby) = (i % recon.mb_cols, i / recon.mb_cols);
                (mb_luma_mean(&src_y, yw, mbx, mby) / 64).min(3) as usize
            } else {
                0
            }
        })
        .collect();

    let mut filter_interior = vec![false; recon.mb_cols * recon.mb_rows];
    let mut is_bpred_map = vec![false; recon.mb_cols * recon.mb_rows];
    // Pass 1 decides and reconstructs; nothing is written yet. Every decision below depends only on
    // the source, the reconstruction, and the quantizer — never on a probability — which is exactly
    // why the frame's coefficient probabilities can be measured afterwards and still describe the
    // stream that gets written.
    let mut records: Vec<MbRecord> = Vec::with_capacity(recon.mb_cols * recon.mb_rows);
    for mb_y in 0..recon.mb_rows {
        for mb_x in 0..recon.mb_cols {
            let segment = segment_map[mb_y * recon.mb_cols + mb_x];
            let qf = seg_qf[segment];
            let uv_mode = select_chroma_mode(&recon, &src_u, &src_v, cw, mb_x, mb_y);
            let u_pred = predict_chroma(&recon.u, recon.c_stride(), mb_x, mb_y, uv_mode);
            let v_pred = predict_chroma(&recon.v, recon.c_stride(), mb_x, mb_y, uv_mode);

            // Whole-block luma candidate and its prediction SAD.
            let wb_mode = select_luma_mode(&recon, &src_y, yw, mb_x, mb_y);
            let wb_sad = block_sad(
                &predict_luma(&recon, mb_x, mb_y, wb_mode),
                &src_y,
                yw,
                mb_x,
                mb_y,
                16,
            );

            // B_PRED candidate — scribbles its reconstruction into recon.y while selecting
            // submodes, so it is only run when the rung allows it. Searching ten submodes across
            // sixteen subblocks is the most expensive thing the encoder does, and the fast rungs
            // buy their speed almost entirely by skipping it.
            let consider_bpred = match tools.bpred {
                Bpred::Off => false,
                // A macroblock the whole-block modes already predict well is very unlikely to
                // profit from 4x4 prediction, so the gate skips the search there. The threshold is
                // per-pixel mean absolute error, scaled by the quantizer because coarser
                // quantization makes small prediction gains irrelevant.
                Bpred::Gated => bpred_gate_admits(wb_sad),
                Bpred::Always => true,
            };
            let (sub_modes, bpred_levels, bpred_sad) = if consider_bpred {
                let above_right = above_right_source(&recon, mb_x, mb_y);
                encode_bpred_luma(
                    &mut recon,
                    &src_y,
                    yw,
                    mb_x,
                    mb_y,
                    &qf,
                    tools.quant_bias,
                    &above_right,
                )
            } else {
                ([B_DC_PRED; 16], [[0i16; 16]; 16], u32::MAX)
            };
            let use_bpred = consider_bpred && bpred_beats_whole_block(bpred_sad, wb_sad);

            let mut levels = MbLevels {
                u: quantize_chroma(&src_u, cw, mb_x, mb_y, &u_pred, &qf, tools.quant_bias),
                v: quantize_chroma(&src_v, cw, mb_x, mb_y, &v_pred, &qf, tools.quant_bias),
                ..Default::default()
            };
            // Compute the luma levels before writing modes so the skip flag — which precedes the luma
            // mode — reflects the whole macroblock. Whole-block luma is reconstructed afterward (B_PRED
            // was already reconstructed during submode selection).
            let wb_pred = (!use_bpred).then(|| predict_luma(&recon, mb_x, mb_y, wb_mode));
            if let Some(yp) = &wb_pred {
                quantize_luma(
                    &src_y,
                    yw,
                    mb_x,
                    mb_y,
                    yp,
                    &qf,
                    tools.quant_bias,
                    &mut levels,
                );
            } else {
                levels.y = bpred_levels;
            }
            let skip = !mb_has_coeffs(&levels);
            let y_mode = if use_bpred { B_PRED } else { wb_mode };

            if let Some(yp) = &wb_pred {
                reconstruct_luma(&mut recon, mb_x, mb_y, yp, &levels, &qf);
            }
            let cstride = recon.c_stride();
            reconstruct_chroma(&mut recon.u, cstride, mb_x, mb_y, &u_pred, &levels.u, &qf);
            reconstruct_chroma(&mut recon.v, cstride, mb_x, mb_y, &v_pred, &levels.v, &qf);

            filter_interior[mb_y * recon.mb_cols + mb_x] = use_bpred || mb_has_coeffs(&levels);
            is_bpred_map[mb_y * recon.mb_cols + mb_x] = use_bpred;
            records.push(MbRecord {
                levels,
                sub_modes,
                y_mode,
                wb_mode,
                uv_mode,
                segment,
                skip,
            });
        }
    }

    // Between the passes: measure what pass 1 actually produced, so the header can describe it.
    if tools.measured_skip_prob {
        header.prob_skip_false = measured_skip_prob(&records);
    }
    let probs = if tools.two_pass_probs {
        optimize_coeff_probs(&tally_coeff_bits(&records, recon.mb_cols))
    } else {
        tokens::DEFAULT_COEFF_PROBS
    };

    // Pass 2 writes. The header goes first because it carries the probabilities and the skip
    // probability the mode and token bits are coded against.
    let mut modes = BoolEncoder::new();
    header::write_frame_header(&mut modes, &header, &probs);
    let mut residuals: Vec<BoolEncoder> = (0..n).map(|_| BoolEncoder::new()).collect();
    let mut above = vec![EntropyCtx::default(); recon.mb_cols];
    let mut above_bmodes = vec![[B_DC_PRED; 4]; recon.mb_cols];
    for mb_y in 0..recon.mb_rows {
        let mut left = EntropyCtx::default();
        let mut left_bmodes = [B_DC_PRED; 4];
        for mb_x in 0..recon.mb_cols {
            let record = &records[mb_y * recon.mb_cols + mb_x];
            let use_bpred = record.y_mode == B_PRED;

            if header.segmentation.update_map {
                modes.put_tree(
                    MB_SEGMENT_TREE,
                    &header.segmentation.tree_probs,
                    record.segment,
                );
            }
            modes.put_bool(header.prob_skip_false, record.skip);
            modes.put_tree(
                prediction::KF_YMODE_TREE,
                &prediction::KF_YMODE_PROB,
                record.y_mode,
            );
            if use_bpred {
                write_bmodes(
                    &mut modes,
                    &record.sub_modes,
                    &above_bmodes[mb_x],
                    &left_bmodes,
                );
            }
            modes.put_tree(
                prediction::KF_UV_MODE_TREE,
                &prediction::KF_UV_MODE_PROB,
                record.uv_mode,
            );

            if record.skip {
                clear_mb_context(&mut above[mb_x], &mut left, use_bpred);
            } else {
                encode_mb_tokens(
                    &mut residuals[mb_y % n],
                    &mut above[mb_x],
                    &mut left,
                    &probs,
                    &record.levels,
                    use_bpred,
                );
            }

            (above_bmodes[mb_x], left_bmodes) =
                bmode_propagation(use_bpred, record.wb_mode, &record.sub_modes);
        }
    }

    apply_loop_filter(
        &mut recon,
        &header.loop_filter,
        &header.segmentation,
        &segment_map,
        &filter_interior,
        &is_bpred_map,
    );

    let part0 = modes.finish();
    let token_parts: Vec<Vec<u8>> = residuals.into_iter().map(BoolEncoder::finish).collect();
    let mut out = Vec::new();
    let part0_len = u32::try_from(part0.len()).unwrap_or(u32::MAX);
    header::write_uncompressed_chunk(&header, part0_len, &mut out)?;
    out.extend_from_slice(&part0);
    // The first N-1 token-partition sizes are stored as 3-byte little-endian prefixes (§9.5); the
    // last partition's size is implied by the remainder — which is why only the first N-1 are
    // bounded here.
    for part in &token_parts[..n - 1] {
        let len = token_partition_size(part.len())?;
        // The low three bytes of the little-endian `u32`, rather than three hand-cut shifts. The
        // shifted form put each byte in its own lane, where the operator has slack no test could
        // see: with every partition under 64 KiB the top byte is zero either way (#110). This is
        // `split_token_partitions`' read, spelled the same way round.
        out.extend_from_slice(&len.to_le_bytes()[..3]);
    }
    for part in &token_parts {
        out.extend_from_slice(part);
    }
    Ok((out, recon))
}

/// The 3-byte little-endian size prefix for a non-final token partition (RFC 6386 §9.5).
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when `len`
/// exceeds [`MAX_TOKEN_PARTITION_SIZE`], which the prefix cannot describe. Split out from the
/// writer so the ceiling is reachable from a test without building a 16 MiB partition.
fn token_partition_size(len: usize) -> Result<u32> {
    match u32::try_from(len) {
        Ok(len) if len <= MAX_TOKEN_PARTITION_SIZE => Ok(len),
        _ => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: token partition exceeds the 3-byte size prefix",
        )),
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;

    use super::*;
    use crate::vp8::decoder::decode_frame;
    use crate::vp8::frame::fixtures::{banded, chroma_dims, yuv420};

    #[test]
    fn encode_refuses_planes_that_are_not_four_two_zero() {
        // VP8 codes 4:2:0 only (RFC 6386 §9.2); a `Planar8` can carry any layout, so the encoder
        // checks it rather than reading the chroma planes at the wrong geometry.
        let dims = Dimensions::new(16, 16).unwrap();
        for subsampling in [
            ChromaSubsampling::Cs444,
            ChromaSubsampling::Cs422,
            ChromaSubsampling::Cs440,
            ChromaSubsampling::Cs400,
        ] {
            let (cw, ch) = subsampling.chroma_dimensions(16, 16);
            let chroma = vec![128; (cw * ch) as usize];
            let planes = Planar8::from_planes_subsampled(
                dims,
                subsampling,
                [vec![0; 256], chroma.clone(), chroma],
            )
            .unwrap();
            let Err(err) = encode_frame(&planes, 40) else {
                panic!("VP8 is 4:2:0 only, but {subsampling:?} encoded");
            };
            assert_eq!(
                err.kind(),
                gamut_core::ErrorKind::InvalidInput,
                "{subsampling:?}"
            );
        }
        // The same image at 4:2:0 encodes, so the refusal is the layout and nothing else.
        assert!(
            encode_frame(
                &yuv420(16, 16, vec![0; 256], vec![128; 64], vec![128; 64]),
                40
            )
            .is_ok()
        );
    }

    /// The segmented stream is pinned byte for byte, because nothing else can see the segment
    /// each macroblock was assigned.
    ///
    /// `segment_map[mb_y * mb_cols + mb_x]` can be indexed with `mb_y / mb_cols` instead and every
    /// consistency check still passes (#110). The segment the encoder used is recorded in
    /// `MbRecord::segment` and transmitted from there, so the stream stays internally coherent:
    /// both decoders read the same declared map, agree with each other, and agree with the
    /// encoder's reconstruction. The loop filter does read the true map, but this configuration
    /// sets `filter_strength: [0; 4]`, so that cannot separate them either.
    ///
    /// What changes is only *which quantizer each macroblock got* -- a worse encode that is still
    /// a valid one. That is the same blind spot the tiff codecs had, and it needs the same answer:
    /// assert the output, not its self-consistency.
    ///
    /// Re-pinning is expected when the encoder legitimately improves; the commit that moves this
    /// number says why. Same contract as `tests/default_bytes.rs`.
    /// Pinned from the current encoder; see `segmented_stream_is_byte_stable`.
    const SEGMENTED_GOLDEN_LEN: usize = 875;
    /// FNV-1a over the same bytes.
    const SEGMENTED_GOLDEN_DIGEST: u64 = 2_735_266_621_437_739_432;

    #[test]
    fn segmented_stream_is_byte_stable() {
        let opts = EncodeOptions {
            segmented: true,
            ..EncodeOptions::default()
        };
        // Banded content over three macroblock rows, so the segment genuinely varies by row.
        let (bits, _) = encode_frame_filtered(&banded(64, 64), 40, opts).expect("encode");

        let digest = bits.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        assert_eq!(
            (bits.len(), digest),
            (SEGMENTED_GOLDEN_LEN, SEGMENTED_GOLDEN_DIGEST),
            "segmented encode drifted"
        );
    }

    /// Position-weighted checksum: changes if any value *or* its index changes, so it pins both the
    /// magnitudes and the layout of a coefficient block in a single comparison.
    fn weighted_checksum(values: impl IntoIterator<Item = i16>) -> i64 {
        values
            .into_iter()
            .enumerate()
            .map(|(i, v)| (i as i64 + 1) * i64::from(v))
            .sum()
    }

    #[test]
    fn quantize_luma_pins_transform_and_quantization() {
        // A fixed source macroblock and (varying) prediction, quantized at a known factor. fdct +
        // fwht + per-band quantize are deterministic, so the exact Y2/AC levels pin the sub-block
        // indexing (`i % 4`, `i / 4`), the block and prediction read offsets, and the residual
        // subtraction — none of which a symmetric encode→decode round-trip can catch.
        let mut src = [0u8; 1024]; // a 32×32 plane, so a non-zero macroblock position is exercised
        for (i, s) in src.iter_mut().enumerate() {
            *s = ((i * 13 + 7) % 251) as u8;
        }
        let mut pred = [0u8; 256];
        for (i, p) in pred.iter_mut().enumerate() {
            *p = ((i * 7 + 17) % 251) as u8;
        }
        let qf = QuantFactors::new(16, &QuantIndices::default());
        let mut levels = MbLevels::default();
        // Quantize macroblock (1, 1) so the `mb_x * 16` and `mb_y * 16` read offsets are non-zero.
        quantize_luma(&src, 32, 1, 1, &pred, &qf, QuantBias::Nearest, &mut levels);
        assert_eq!(weighted_checksum(levels.y2), 707);
        assert_eq!(weighted_checksum(levels.y.into_iter().flatten()), -22284);
    }

    #[test]
    fn quantize_chroma_pins_transform_and_quantization() {
        // Same idea for the four chroma sub-blocks: the exact levels pin the prediction read offset.
        let mut src = [0u8; 64];
        for (i, s) in src.iter_mut().enumerate() {
            *s = ((i * 11 + 5) % 251) as u8;
        }
        let mut pred = [0u8; 64];
        for (i, p) in pred.iter_mut().enumerate() {
            *p = ((i * 5 + 3) % 251) as u8;
        }
        let qf = QuantFactors::new(16, &QuantIndices::default());
        let levels = quantize_chroma(&src, 8, 0, 0, &pred, &qf, QuantBias::Nearest);
        assert_eq!(weighted_checksum(levels.into_iter().flatten()), -1085);
    }

    #[test]
    fn encode_bpred_luma_pins_residual() {
        // The per-subblock residual (source minus the chosen submode prediction) feeds these levels;
        // flipping the subtraction to addition changes the coefficients.
        let mut recon = FrameBuffers::new(16, 16);
        let mut src = [0u8; 256];
        for (i, s) in src.iter_mut().enumerate() {
            *s = ((i * 11 + 3) % 251) as u8;
        }
        let qf = QuantFactors::new(12, &QuantIndices::default());
        let (_, levels, _) =
            encode_bpred_luma(&mut recon, &src, 16, 0, 0, &qf, QuantBias::Nearest, &[0; 4]);
        assert_eq!(weighted_checksum(levels.into_iter().flatten()), -3083);
    }

    #[test]
    fn frame_header_carries_quant_index_and_filter_type() {
        // The minimal key-frame header must carry the requested base quantizer and filter type:
        // deleting either struct field would silently fall back to the default (y_ac 0 / normal).
        let simple = frame_header(176, 144, 100, true);
        assert_eq!(
            simple.quant.y_ac, 100,
            "base quantizer index must be stored"
        );
        assert!(
            simple.loop_filter.simple,
            "the simple-filter request must be stored"
        );
        assert!(
            !frame_header(176, 144, 100, false).loop_filter.simple,
            "a normal-filter request must not set the simple flag"
        );
    }

    #[test]
    fn interior_edges_filter_on_coefficients_not_only_bpred() {
        // A gentle quadratic bowl: smooth enough that whole-block prediction is chosen (not B_PRED)
        // yet curved enough to leave a residual, so macroblocks carry coefficients. The §15.1 rule
        // then filters their interior subblock edges (`use_bpred || mb_has_coeffs`); `||` flipped to
        // `&&` would skip it for these non-B_PRED macroblocks, desyncing the encoder reconstruction
        // from the decoder. (A strong filter level — q = 40 — makes the divergence observable.)
        let (w, h) = (32u32, 32u32);
        let y: Vec<u8> = (0..(w * h) as usize)
            .map(|i| {
                let (x, yy) = (i % w as usize, i / w as usize);
                (128 + (x * x + yy * yy) / 64) as u8
            })
            .collect();
        let (cw, ch) = chroma_dims(w, h);
        let yuv = yuv420(w, h, y, vec![128; cw * ch], vec![128; cw * ch]);
        let (bits, recon) = encode_frame_filtered(&yuv, 40, EncodeOptions::default())
            .expect("fixture fits the partition-size fields");
        let dec = decode_frame(&bits).expect("decode");
        assert_eq!(
            recon.to_yuv420().unwrap().planes()[0],
            dec.to_yuv420().unwrap().planes()[0]
        );
    }

    /// The 3-byte size prefix (RFC 6386 §9.5) tops out at 16 MiB - 1, and the writer packs the
    /// value into exactly those three bytes — so a length one past the ceiling would be written
    /// truncated, pointing the decoder at a boundary that is not there. Both sides of the edge are
    /// pinned, and the constant is checked against the field it describes rather than against
    /// itself.
    #[test]
    fn token_partition_size_stops_at_the_three_byte_prefix() {
        // The ceiling is what three little-endian bytes can hold, derived independently.
        assert_eq!(
            MAX_TOKEN_PARTITION_SIZE,
            u32::from_le_bytes([0xFF, 0xFF, 0xFF, 0])
        );
        assert_eq!(token_partition_size(0).expect("empty"), 0);
        let max = MAX_TOKEN_PARTITION_SIZE as usize;
        assert_eq!(
            token_partition_size(max).expect("the ceiling"),
            MAX_TOKEN_PARTITION_SIZE
        );
        let err = token_partition_size(max + 1).expect_err("one past the ceiling");
        assert!(
            err.to_string().contains("token partition"),
            "unexpected error: {err}"
        );
        // And a length that does not even fit `u32` is the same refusal, not a wrapped success.
        let err = token_partition_size(usize::MAX).expect_err("absurd length");
        assert!(err.to_string().contains("token partition"));
    }

    /// The probability optimizer only adopts a measured value when it pays for its own update
    /// record: the "yes, update" flag plus eight literal bits, which is 2048 cost units on top of
    /// re-coding every token. That trade is the entire point of the function, so it is pinned at
    /// its boundary from both sides — a context whose saving clears the record cost adopts, and an
    /// otherwise identical context whose saving does not clear it keeps the default.
    ///
    /// Both cases use one context of the frame, and the counts are chosen so the *only* thing
    /// separating them is the size of the saving; a mis-sized record cost, or an adopt/reject
    /// comparison that admits ties, moves one of them.
    #[test]
    fn probability_updates_must_pay_for_their_own_record() {
        /// Cost, in 1/256 bit, of coding `zeros`/`ones` at probability `p`.
        fn coding_cost(zeros: u64, ones: u64, p: u8) -> u64 {
            zeros * u64::from(bit_cost(false, p)) + ones * u64::from(bit_cost(true, p))
        }
        // Find a context whose default probability is far from an even split, so a measured 50/50
        // has something to save.
        let (plane, band, ctx, node) = (0, 1, 0, 0);
        let old = tokens::DEFAULT_COEFF_PROBS[plane][band][ctx][node];
        let update_prob = tokens::COEFF_UPDATE_PROBS[plane][band][ctx][node];
        // Saving of the measured probability over the default, at an even split of `n` each way.
        let saving = |n: u64| -> i64 {
            let new = 127u8;
            let keep = coding_cost(n, n, old) + u64::from(bit_cost(false, update_prob));
            let adopt = coding_cost(n, n, new) + u64::from(bit_cost(true, update_prob)) + 8 * 256;
            keep as i64 - adopt as i64
        };
        // Grow the count until adopting is worth it; `n - 1` is then the last count that is not.
        let mut n = 1u64;
        while saving(n) <= 0 && n < 1 << 20 {
            n += 1;
        }
        assert!(n > 1, "the boundary must be interior to the search");
        let mut counts: tokens::CoeffCounts =
            [[[[[0; 2]; tokens::ENTROPY_NODES]; 3]; tokens::COEFF_BANDS]; tokens::PLANE_TYPES];

        // Just over the boundary: adopted.
        counts[plane][band][ctx][node] = [n as u32, n as u32];
        assert_eq!(
            optimize_coeff_probs(&counts)[plane][band][ctx][node],
            127,
            "a saving that clears the update record must be adopted"
        );

        // Just under it: the default survives.
        counts[plane][band][ctx][node] = [(n - 1) as u32, (n - 1) as u32];
        assert_eq!(
            optimize_coeff_probs(&counts)[plane][band][ctx][node],
            old,
            "a saving that does not clear the update record must be rejected"
        );
    }

    /// Both `B_PRED` decisions are inequalities, and an inequality is only pinned by the value
    /// that sits exactly on it. The gate admits a macroblock whose whole-block prediction misses
    /// by *more* than the per-pixel threshold, and the 4x4 mode wins only by coming in *under*
    /// the whole-block cost — a tie keeps the whole-block mode, which is cheaper to signal.
    #[test]
    fn bpred_decisions_are_strict_at_their_thresholds() {
        let threshold = BPRED_GATE_SAD_PER_PIXEL * 256;
        assert!(!bpred_gate_admits(threshold - 1));
        assert!(
            !bpred_gate_admits(threshold),
            "the threshold itself is not admitted"
        );
        assert!(bpred_gate_admits(threshold + 1));

        // A tie goes to the whole-block mode.
        let wb = 5_000;
        assert!(bpred_beats_whole_block(wb - BPRED_SAD_PENALTY - 1, wb));
        assert!(
            !bpred_beats_whole_block(wb - BPRED_SAD_PENALTY, wb),
            "an exact tie must keep the whole-block mode"
        );
        assert!(!bpred_beats_whole_block(wb - BPRED_SAD_PENALTY + 1, wb));
    }

    /// A saving that exactly equals the update record's cost is not a saving: the record buys
    /// nothing and the default survives. The optimizer's comparison is strict for that reason, and
    /// a tie is the only input that tells a strict comparison from a permissive one.
    ///
    /// The tie is solved for rather than hardcoded — at an even split the measured probability is
    /// 127, whose per-event cost is exactly 256 either way, so the boundary count is
    /// `(update_yes + record - update_no) / (default_cost - 512)` wherever that divides exactly.
    /// If a future probability table leaves no context with an exact solution the search says so
    /// instead of passing vacuously.
    #[test]
    fn a_probability_update_that_exactly_breaks_even_is_not_taken() {
        const RECORD: u64 = 8 * 256;
        let mut solved = None;
        'search: for plane in 0..tokens::PLANE_TYPES {
            for band in 0..tokens::COEFF_BANDS {
                for ctx in 0..3 {
                    for node in 0..tokens::ENTROPY_NODES {
                        let old = tokens::DEFAULT_COEFF_PROBS[plane][band][ctx][node];
                        if old == 127 {
                            continue;
                        }
                        let up = tokens::COEFF_UPDATE_PROBS[plane][band][ctx][node];
                        let per_event_old =
                            u64::from(bit_cost(false, old)) + u64::from(bit_cost(true, old));
                        // The measured 127 costs exactly one bit each way: 256 + 256.
                        let Some(gain) = per_event_old.checked_sub(512) else {
                            continue;
                        };
                        if gain == 0 {
                            continue;
                        }
                        let need =
                            u64::from(bit_cost(true, up)) + RECORD - u64::from(bit_cost(false, up));
                        if need.is_multiple_of(gain)
                            && need / gain > 0
                            && need / gain < u64::from(u32::MAX)
                        {
                            solved = Some((plane, band, ctx, node, old, (need / gain) as u32));
                            break 'search;
                        }
                    }
                }
            }
        }
        let (plane, band, ctx, node, old, n) =
            solved.expect("some context must admit an exact break-even count");

        let mut counts: tokens::CoeffCounts =
            [[[[[0; 2]; tokens::ENTROPY_NODES]; 3]; tokens::COEFF_BANDS]; tokens::PLANE_TYPES];
        counts[plane][band][ctx][node] = [n, n];
        assert_eq!(
            optimize_coeff_probs(&counts)[plane][band][ctx][node],
            old,
            "breaking even must keep the default, not spend a record to buy nothing"
        );
        // One event more and the saving is real, so the same context does adopt — which is what
        // shows the tie above is the boundary and not simply an unreachable context.
        counts[plane][band][ctx][node] = [n + 1, n + 1];
        assert_eq!(
            optimize_coeff_probs(&counts)[plane][band][ctx][node],
            127,
            "one event past break-even must adopt"
        );
    }

    /// The probability optimizer accumulates frame-wide token tallies, so its cost arithmetic must
    /// survive counts a large frame really produces. A 4000x4000 encode puts well over two million
    /// events into a single hot context, and at `bit_cost`'s maximum of 2048 units each the product
    /// leaves `u32` — which used to abort the encode under overflow checks (and silently invert the
    /// adopt/reject decision without them). Driving one context past that boundary pins the `u64`
    /// accumulator without needing a sixteen-megapixel fixture.
    #[test]
    fn probability_costs_survive_large_frame_counts() {
        // Above u32::MAX / 2048, so any context whose default probability is extreme overflows.
        const HUGE: u32 = 4_000_000;
        let mut counts: tokens::CoeffCounts =
            [[[[[0; 2]; tokens::ENTROPY_NODES]; 3]; tokens::COEFF_BANDS]; tokens::PLANE_TYPES];
        for plane in counts.iter_mut() {
            for band in plane.iter_mut() {
                for ctx in band.iter_mut() {
                    for node in ctx.iter_mut() {
                        *node = [HUGE, HUGE];
                    }
                }
            }
        }
        let probs = optimize_coeff_probs(&counts);
        // Every context saw an even split, so the measured probability is 127 wherever adopting it
        // pays for the update record — and nothing may be left at a wildly mispredicting default.
        assert!(
            probs
                .iter()
                .flatten()
                .flatten()
                .flatten()
                .any(|&p| p == 127),
            "an even split must be adopted somewhere"
        );
        // The counts are symmetric, so no adopted value may sit outside the codable range.
        assert!(probs.iter().flatten().flatten().flatten().all(|&p| p >= 1));
    }
}
