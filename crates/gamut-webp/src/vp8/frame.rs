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

// The macroblock/block math indexes several fixed-size arrays in lock-step (and over partial ranges
// like `1..16`), where explicit indices read closer to the spec than iterator adaptors.
#![allow(clippy::needless_range_loop)]

use gamut_color::{Yuv420, clip_pixel8};
use gamut_core::{Error, Result};

use super::bool_coder::{BoolDecoder, BoolEncoder};
use super::cost::bit_cost;
use super::effort::{Bpred, EFFORT_TABLE, QuantBias};
/// Re-exported so the low-level frame API carries the loop-filter delta type next to [`EncodeOptions`].
pub use super::header::LoopFilterDeltas;
use super::header::{
    self, LoopFilterParams, QuantIndices, Segmentation, UNCOMPRESSED_CHUNK_LEN, Vp8FrameHeader,
};
use super::loop_filter;
use super::prediction::{self, B_DC_PRED, B_PRED, DC_PRED, H_PRED, NUM_BMODES, TM_PRED, V_PRED};
use super::quant::{self, QuantFactors};
use super::tokens::{self, CoeffProbs};
use super::transform::{fdct4x4, fwht4x4, idct4x4, iwht4x4};
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

/// The largest non-final token partition the bitstream can describe: its size is a 3-byte
/// little-endian prefix (RFC 6386 §9.5), so 16 MiB - 1. The final partition carries no prefix —
/// its length is the remainder — so it is unbounded.
pub const MAX_TOKEN_PARTITION_SIZE: u32 = (1 << 24) - 1;

/// Segment-id coding tree (RFC 6386 §10 `mb_segment_tree`): four leaves over two boolean decisions.
const MB_SEGMENT_TREE: &[i8] = &[2, 4, 0, -1, -2, -3];

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

/// The clamped base quantizer index for segment `s` (RFC 6386 §9.3/§10): the absolute or
/// delta-adjusted value when segmentation is enabled, else the frame base.
fn segment_q_index(seg: &Segmentation, base_y_ac: u8, s: usize) -> i32 {
    if !seg.enabled {
        return i32::from(base_y_ac);
    }
    let q = if seg.abs_delta {
        i32::from(seg.quantizer[s])
    } else {
        i32::from(base_y_ac) + i32::from(seg.quantizer[s])
    };
    q.clamp(0, 127)
}

/// The four per-segment quantizer factor sets for a frame (all equal when segmentation is disabled).
fn segment_quant_factors(header: &Vp8FrameHeader) -> [QuantFactors; 4] {
    core::array::from_fn(|s| {
        let base_q = segment_q_index(&header.segmentation, header.quant.y_ac, s);
        QuantFactors::new(base_q, &header.quant)
    })
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

/// Per-macroblock-column entropy context: whether the prior block in each position carried at least
/// one non-zero coefficient (RFC 6386 §13.3). A single instance also serves as the running "left"
/// context, reset at the start of each macroblock row.
#[derive(Clone, Copy, Default)]
struct EntropyCtx {
    /// Y2 (luma-DC WHT) block.
    y2: bool,
    /// The four luma sub-block columns (above) / rows (left).
    y: [bool; 4],
    /// The two U sub-block columns / rows.
    u: [bool; 2],
    /// The two V sub-block columns / rows.
    v: [bool; 2],
}

/// One macroblock's quantized coefficient levels: the Y2 block, 16 luma sub-blocks, 4 U and 4 V.
#[derive(Clone, Default)]
struct MbLevels {
    y2: [i16; 16],
    y: [[i16; 16]; 16],
    u: [[i16; 16]; 4],
    v: [[i16; 16]; 4],
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

/// Macroblock-aligned reconstructed YUV planes (luma `mb_cols*16 × mb_rows*16`, chroma half each).
pub struct FrameBuffers {
    width: u32,
    height: u32,
    mb_cols: usize,
    mb_rows: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl FrameBuffers {
    fn new(width: u32, height: u32) -> Self {
        let mb_cols = (width as usize).div_ceil(16);
        let mb_rows = (height as usize).div_ceil(16);
        Self {
            width,
            height,
            mb_cols,
            mb_rows,
            y: vec![0u8; mb_cols * 16 * mb_rows * 16],
            u: vec![0u8; mb_cols * 8 * mb_rows * 8],
            v: vec![0u8; mb_cols * 8 * mb_rows * 8],
        }
    }

    fn y_stride(&self) -> usize {
        self.mb_cols * 16
    }

    fn c_stride(&self) -> usize {
        self.mb_cols * 8
    }

    /// Crops the reconstruction to a visible-resolution [`Yuv420`].
    #[must_use]
    pub fn to_yuv420(&self) -> Yuv420 {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (
            Yuv420::chroma_width(self.width) as usize,
            Yuv420::chroma_height(self.height) as usize,
        );
        let crop = |plane: &[u8], stride: usize, pw: usize, ph: usize| {
            let mut out = vec![0u8; pw * ph];
            for row in 0..ph {
                out[row * pw..row * pw + pw]
                    .copy_from_slice(&plane[row * stride..row * stride + pw]);
            }
            out
        };
        let y = crop(&self.y, self.y_stride(), w, h);
        let u = crop(&self.u, self.c_stride(), cw, ch);
        let v = crop(&self.v, self.c_stride(), cw, ch);
        Yuv420::new(self.width, self.height, y, u, v).expect("cropped planes match dimensions")
    }
}

/// Picks a loop-filter strength from the base quantizer — stronger quantization deblocks harder. A
/// coarse heuristic (true filter-level selection is part of issue #32); a level of 0 disables it.
fn filter_level(quant_index: u8) -> u8 {
    quant_index / 2
}

/// The clamped loop-filter level for segment `s` (RFC 6386 §10/§15.4): the segment's absolute or
/// delta-adjusted filter strength when segmentation is enabled, else the frame base level.
fn segment_filter_level(base: u8, seg: &Segmentation, s: usize) -> u8 {
    if !seg.enabled {
        return base;
    }
    let level = if seg.abs_delta {
        i32::from(seg.filter_strength[s])
    } else {
        i32::from(base) + i32::from(seg.filter_strength[s])
    };
    level.clamp(0, 63) as u8
}

/// Applies the frame's configured loop filter to the reconstruction as a final whole-frame pass: the
/// simple filter deblocks luma only, the normal filter luma and chroma. Each macroblock is filtered at
/// its segment's level (uniform when segmentation is disabled); an all-zero level set is a no-op.
fn apply_loop_filter(
    recon: &mut FrameBuffers,
    lf: &LoopFilterParams,
    seg: &Segmentation,
    segment_map: &[usize],
    filter_interior: &[bool],
    is_bpred: &[bool],
) {
    // A zero frame-level filter level disables the loop filter for the whole frame (RFC 6386 §9.4;
    // matches libwebp's `filter_type = 0`), regardless of any per-segment or per-macroblock delta.
    if lf.level == 0 {
        return;
    }
    // Per-macroblock level: the segment-adjusted base plus the §9.4 deltas. Key frames are all-intra,
    // so the intra reference-frame delta (`ref_frame[0]`) applies to every macroblock and the B_PRED
    // mode delta (`mode[0]`) to 4×4-predicted ones; the sum is clamped to [0, 63].
    let mb_level: Vec<u8> = segment_map
        .iter()
        .zip(is_bpred)
        .map(|(&s, &bpred)| {
            let mut level = i32::from(segment_filter_level(lf.level, seg, s));
            level += i32::from(lf.deltas.ref_frame[0]);
            if bpred {
                level += i32::from(lf.deltas.mode[0]);
            }
            level.clamp(0, 63) as u8
        })
        .collect();
    if mb_level.iter().all(|&l| l == 0) {
        return;
    }
    let (ys, cs, mbc, mbr) = (
        recon.y_stride(),
        recon.c_stride(),
        recon.mb_cols,
        recon.mb_rows,
    );
    if lf.simple {
        loop_filter::simple_filter_luma(
            &mut recon.y,
            ys,
            mbc,
            mbr,
            &mb_level,
            lf.sharpness,
            filter_interior,
        );
    } else {
        loop_filter::normal_filter(
            &mut recon.y,
            &mut recon.u,
            &mut recon.v,
            ys,
            cs,
            mbc,
            mbr,
            &mb_level,
            lf.sharpness,
            filter_interior,
        );
    }
}

/// Whether a macroblock carries any non-zero quantized coefficient — the second half of the
/// loop-filter interior-edge skip rule (RFC 6386 §15.1).
fn mb_has_coeffs(levels: &MbLevels) -> bool {
    levels.y2.iter().any(|&x| x != 0)
        || levels.y.iter().flatten().any(|&x| x != 0)
        || levels.u.iter().flatten().any(|&x| x != 0)
        || levels.v.iter().flatten().any(|&x| x != 0)
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

/// Resets a macroblock's coefficient context to "no non-zero coefficients" for a skipped macroblock
/// (RFC 6386 §11.1): equivalent to coding all-zero blocks, but the `B_PRED` Y2 context persists since
/// such a macroblock carries no Y2 block.
fn clear_mb_context(above: &mut EntropyCtx, left: &mut EntropyCtx, is_bpred: bool) {
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

/// Reconstructs a skipped `B_PRED` macroblock's luma: each subblock is its prediction with no residual
/// (the encoder's all-zero-coefficient reconstruction).
fn reconstruct_bpred_zero(
    recon: &mut FrameBuffers,
    mb_x: usize,
    mb_y: usize,
    sub_modes: &[usize; 16],
    above_right: &[u8; 4],
) {
    let (px, py, rstride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let (sx, sy) = (px + c * 4, py + r * 4);
        let (a, l, corner) = subblock_neighbors(recon, sx, sy, c, above_right);
        let pred = prediction::subblock_predict(sub_modes[i], &a, &l, corner);
        let pred_i16: [i16; 16] = core::array::from_fn(|k| i16::from(pred[k]));
        write_block(&mut recon.y, rstride, sx, sy, &pred_i16, &[0i16; 16]);
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

/// Gathers the `n`-pixel row at `(x, y)` of `plane` into a fixed buffer (only `[..n]` is meaningful).
fn row_at(plane: &[u8], stride: usize, x: usize, y: usize, n: usize) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..n].copy_from_slice(&plane[y * stride + x..y * stride + x + n]);
    b
}

/// Gathers the `n`-pixel column at `(x, y)` of `plane` into a fixed buffer.
fn col_at(plane: &[u8], stride: usize, x: usize, y: usize, n: usize) -> [u8; 16] {
    let mut b = [0u8; 16];
    for (r, slot) in b[..n].iter_mut().enumerate() {
        *slot = plane[(y + r) * stride + x];
    }
    b
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

/// Extracts the 4×4 sub-block at `(sub_x, sub_y)` of a `stride`-wide prediction block, as 16-bit.
fn sub_pred(pred: &[u8], stride: usize, sub_x: usize, sub_y: usize) -> [i16; 16] {
    let mut out = [0i16; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = i16::from(pred[(sub_y + r) * stride + sub_x + c]);
        }
    }
    out
}

/// Writes `clip_pixel8(pred + residue)` into the 4×4 block at `(x, y)` of `plane`.
fn write_block(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    pred: &[i16; 16],
    residue: &[i16; 16],
) {
    for r in 0..4 {
        for c in 0..4 {
            let v = i32::from(pred[r * 4 + c]) + i32::from(residue[r * 4 + c]);
            plane[(y + r) * stride + x + c] = clip_pixel8(v);
        }
    }
}

/// The above-left corner pixel for prediction: 127 on the top macroblock row, 129 on the left column,
/// otherwise the reconstructed pixel (RFC 6386 §12.2).
fn corner_pixel(plane: &[u8], stride: usize, px: usize, py: usize, mb_x: usize, mb_y: usize) -> u8 {
    if mb_y == 0 {
        127
    } else if mb_x == 0 {
        129
    } else {
        plane[(py - 1) * stride + px - 1]
    }
}

/// One reconstructed luma pixel, or its off-frame edge value (127 above the frame, 129 to the left).
fn luma_pixel(recon: &FrameBuffers, y: i32, x: i32) -> u8 {
    if y < 0 {
        127
    } else if x < 0 {
        129
    } else {
        recon.y[y as usize * recon.y_stride() + x as usize]
    }
}

/// The four above-right pixels of the macroblock's top-right subblock, shared by all right-column
/// subblocks (RFC 6386 §12.3 `copy_down`). Matching libwebp: 127 on the top row; the next
/// macroblock's top-left four pixels normally; or the current macroblock's last above pixel
/// replicated on the rightmost column (`frame_dec.c`: `memset(top_right, top[15])`).
fn above_right_source(recon: &FrameBuffers, mb_x: usize, mb_y: usize) -> [u8; 4] {
    if mb_y == 0 {
        return [127; 4];
    }
    let stride = recon.y_stride();
    let row = (mb_y * 16 - 1) * stride;
    if mb_x + 1 >= recon.mb_cols {
        [recon.y[row + mb_x * 16 + 15]; 4]
    } else {
        let base = row + mb_x * 16 + 16;
        [
            recon.y[base],
            recon.y[base + 1],
            recon.y[base + 2],
            recon.y[base + 3],
        ]
    }
}

/// Gathers a 4×4 luma subblock's prediction neighbors from the in-place reconstruction: the eight
/// above pixels `A[0..8]` (four above, four above-right), the four left `L[0..4]`, and the above-left
/// corner. `(sx, sy)` is the subblock's top-left in frame coordinates and `c` its column within the
/// macroblock (the right column, `c == 3`, takes its above-right from the shared `above_right`).
fn subblock_neighbors(
    recon: &FrameBuffers,
    sx: usize,
    sy: usize,
    c: usize,
    above_right: &[u8; 4],
) -> ([u8; 8], [u8; 4], u8) {
    let (xi, yi) = (sx as i32, sy as i32);
    let corner = luma_pixel(recon, yi - 1, xi - 1);
    let mut a = [0u8; 8];
    for k in 0..4 {
        a[k] = luma_pixel(recon, yi - 1, xi + k as i32);
    }
    if c == 3 {
        a[4..8].copy_from_slice(above_right);
    } else {
        for k in 0..4 {
            a[4 + k] = luma_pixel(recon, yi - 1, xi + 4 + k as i32);
        }
    }
    let mut l = [0u8; 4];
    for k in 0..4 {
        l[k] = luma_pixel(recon, yi + k as i32, xi - 1);
    }
    (a, l, corner)
}

/// Produces the 16×16 luma prediction for macroblock `(mb_x, mb_y)` under whole-block `mode`.
fn predict_luma(recon: &FrameBuffers, mb_x: usize, mb_y: usize, mode: usize) -> [u8; 256] {
    let (px, py, stride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    let above = (mb_y > 0).then(|| row_at(&recon.y, stride, px, py - 1, 16));
    let left = (mb_x > 0).then(|| col_at(&recon.y, stride, px - 1, py, 16));
    let corner = corner_pixel(&recon.y, stride, px, py, mb_x, mb_y);
    let mut pred = [0u8; 256];
    prediction::predict_block(
        mode,
        16,
        above.as_ref().map(|a| &a[..16]),
        left.as_ref().map(|l| &l[..16]),
        corner,
        &mut pred,
    );
    pred
}

/// Produces the 8×8 prediction for one chroma plane under whole-block `mode`.
fn predict_chroma(plane: &[u8], stride: usize, mb_x: usize, mb_y: usize, mode: usize) -> [u8; 64] {
    let (px, py) = (mb_x * 8, mb_y * 8);
    let above = (mb_y > 0).then(|| row_at(plane, stride, px, py - 1, 8));
    let left = (mb_x > 0).then(|| col_at(plane, stride, px - 1, py, 8));
    let corner = corner_pixel(plane, stride, px, py, mb_x, mb_y);
    let mut pred = [0u8; 64];
    prediction::predict_block(
        mode,
        8,
        above.as_ref().map(|a| &a[..8]),
        left.as_ref().map(|l| &l[..8]),
        corner,
        &mut pred,
    );
    pred
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

/// Reconstructs the 16 luma sub-blocks of a macroblock: the Y2 inverse-WHT supplies each sub-block's
/// DC, the AC levels are dequantized, and `pred + idct` is written into the recon buffer. Shared by
/// the encoder and decoder.
fn reconstruct_luma(
    recon: &mut FrameBuffers,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 256],
    levels: &MbLevels,
    qf: &QuantFactors,
) {
    let mut y2_dq = [0i16; 16];
    y2_dq[0] = quant::dequantize(levels.y2[0], qf.y2_dc);
    for k in 1..16 {
        y2_dq[k] = quant::dequantize(levels.y2[k], qf.y2_ac);
    }
    let dc = iwht4x4(&y2_dq);

    let stride = recon.y_stride();
    for i in 0..16 {
        let mut dq = [0i16; 16];
        dq[0] = dc[i];
        for k in 1..16 {
            dq[k] = quant::dequantize(levels.y[i][k], qf.y1_ac);
        }
        let residue = idct4x4(&dq);
        let (sc, sr) = (i % 4, i / 4);
        write_block(
            &mut recon.y,
            stride,
            mb_x * 16 + sc * 4,
            mb_y * 16 + sr * 4,
            &sub_pred(pred, 16, sc * 4, sr * 4),
            &residue,
        );
    }
}

/// Reconstructs the four sub-blocks of one chroma plane from full (DC+AC) levels.
fn reconstruct_chroma(
    plane: &mut [u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 64],
    levels: &[[i16; 16]; 4],
    qf: &QuantFactors,
) {
    for i in 0..4 {
        let mut dq = [0i16; 16];
        dq[0] = quant::dequantize(levels[i][0], qf.uv_dc);
        for k in 1..16 {
            dq[k] = quant::dequantize(levels[i][k], qf.uv_ac);
        }
        let residue = idct4x4(&dq);
        let (sc, sr) = (i % 2, i / 2);
        write_block(
            plane,
            stride,
            mb_x * 8 + sc * 4,
            mb_y * 8 + sr * 4,
            &sub_pred(pred, 8, sc * 4, sr * 4),
            &residue,
        );
    }
}

/// Transforms + quantizes one luma macroblock against its prediction, returning the Y2 and per
/// sub-block AC levels.
/// Forward-quantizes one coefficient under the effort level's rounding rule.
fn quantize_with(bias: QuantBias, coeff: i16, factor: i16, dead_zone: u16) -> i16 {
    match bias {
        QuantBias::Nearest => quant::quantize(coeff, factor),
        QuantBias::DeadZone => quant::quantize_biased(coeff, factor, dead_zone),
    }
}

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

/// Decodes and reconstructs the luma plane of a `B_PRED` macroblock from its submodes and the token
/// partition, interleaving token decode and reconstruction (each subblock predicts from the one
/// before it) and threading the plane-3 non-zero context. Leaves the Y2 context untouched.
#[allow(clippy::too_many_arguments)] // the reconstruction loop genuinely needs all of this state
fn decode_bpred_luma(
    recon: &mut FrameBuffers,
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    mb_x: usize,
    mb_y: usize,
    qf: &QuantFactors,
    sub_modes: &[usize; 16],
    above_right: &[u8; 4],
) {
    let (px, py, rstride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let (sx, sy) = (px + c * 4, py + r * 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let mut lev = [0i16; 16];
        let has = tokens::decode_block(dec, &mut lev, 3, ctx, probs);
        above.y[c] = has;
        left.y[r] = has;

        let (a, l, corner) = subblock_neighbors(recon, sx, sy, c, above_right);
        let pred = prediction::subblock_predict(sub_modes[i], &a, &l, corner);
        let mut dq = [0i16; 16];
        dq[0] = quant::dequantize(lev[0], qf.y1_dc);
        for k in 1..16 {
            dq[k] = quant::dequantize(lev[k], qf.y1_ac);
        }
        let residue = idct4x4(&dq);
        let pred_i16: [i16; 16] = core::array::from_fn(|k| i16::from(pred[k]));
        write_block(&mut recon.y, rstride, sx, sy, &pred_i16, &residue);
    }
}

/// The above/left subblock-mode context for the `j`th subblock (RFC 6386 §11.3): the mode of the
/// subblock above (within the macroblock for rows > 0, else `above_col`) and to the left (within for
/// columns > 0, else `left_col`).
fn bmode_context(
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

/// Reads the 16 `B_PRED` submodes, mirroring [`write_bmodes`].
fn read_bmodes(
    modes: &mut BoolDecoder,
    above_col: &[usize; 4],
    left_col: &[usize; 4],
) -> [usize; 16] {
    let mut sub_modes = [B_DC_PRED; 16];
    for i in 0..16 {
        let (a, l) = bmode_context(&sub_modes, above_col, left_col, i);
        sub_modes[i] = modes.get_tree(prediction::BMODE_TREE, &prediction::KF_BMODE_PROB[a][l]);
    }
    sub_modes
}

/// The macroblock's bottom-row and right-column subblock modes, to seed the above/left context of the
/// next row/column (RFC 6386 §11.3 caveat 4): the actual submodes for `B_PRED`, else the constant
/// derived from the whole-block luma mode.
fn bmode_propagation(
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

/// Decodes a macroblock's U then V chroma blocks into `levels`, mirroring [`encode_chroma_tokens`].
fn decode_chroma_tokens(
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    levels: &mut MbLevels,
) {
    for (plane_levels, above_ctx, left_ctx) in [
        (&mut levels.u, &mut above.u, &mut left.u),
        (&mut levels.v, &mut above.v, &mut left.v),
    ] {
        for i in 0..4 {
            let (r, c) = (i / 2, i % 2);
            let ctx = usize::from(above_ctx[c]) + usize::from(left_ctx[r]);
            let has = tokens::decode_block(dec, &mut plane_levels[i], 2, ctx, probs);
            above_ctx[c] = has;
            left_ctx[r] = has;
        }
    }
}

/// Decodes a whole-block (non-`B_PRED`) macroblock's coefficient blocks: Y2, 16 luma (plane 0), then
/// chroma.
fn decode_mb_tokens(
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
) -> MbLevels {
    let mut levels = MbLevels::default();
    let ctx = usize::from(above.y2) + usize::from(left.y2);
    let has = tokens::decode_block(dec, &mut levels.y2, 1, ctx, probs);
    above.y2 = has;
    left.y2 = has;
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let has = tokens::decode_block(dec, &mut levels.y[i], 0, ctx, probs);
        above.y[c] = has;
        left.y[r] = has;
    }
    decode_chroma_tokens(dec, above, left, probs, &mut levels);
    levels
}

/// Encodes a [`Yuv420`] image as a VP8 key-frame bitstream (the `VP8 ` chunk payload), returning the
/// bitstream and the encoder's reconstruction (the tier-2 oracle: it must equal any decoder's output).
/// Uses the normal loop filter.
///
/// # Errors
///
/// As [`encode_frame_filtered`].
pub fn encode_frame(yuv: &Yuv420, quant_index: u8) -> Result<(Vec<u8>, FrameBuffers)> {
    encode_frame_filtered(yuv, quant_index, EncodeOptions::default())
}

/// Encodes a frame with explicit [`EncodeOptions`] — the loop-filter type and whether to emit
/// quantizer segments. [`encode_frame`] uses the defaults (normal filter, unsegmented). This lets the
/// differential oracle drive the alternative encoder paths.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] when the coded frame outgrows one of the bitstream's own
/// partition-size fields: the control partition past
/// [`MAX_FIRST_PARTITION_SIZE`](super::header::MAX_FIRST_PARTITION_SIZE) (19 bits, RFC 6386 §9.1),
/// or a non-final token partition past [`MAX_TOKEN_PARTITION_SIZE`] (24 bits, §9.5). Both are
/// format ceilings a large, highly detailed frame can genuinely reach — the mode records of a
/// `B_PRED`-heavy frame are what fill the control partition — and neither can be encoded, so the
/// alternative to reporting them is emitting a stream no decoder accepts. libwebp reports the same
/// condition (`VP8_ENC_ERROR_PARTITION0_OVERFLOW`) rather than degrading quality behind the
/// caller's back, and neither does this. The lever is [`EncodeOptions::effort`]: `B_PRED` is what
/// fills partition 0, so [`Effort::Fastest`] (which never emits it) encodes the whole WebP canvas
/// range, up to 16383x16383, at any detail level.
pub fn encode_frame_filtered(
    yuv: &Yuv420,
    quant_index: u8,
    opts: EncodeOptions,
) -> Result<(Vec<u8>, FrameBuffers)> {
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
    let src_y = pad_plane(yuv.y(), yuv.width() as usize, yuv.height() as usize, yw, yh);
    let vcw = Yuv420::chroma_width(yuv.width()) as usize;
    let vch = Yuv420::chroma_height(yuv.height()) as usize;
    let src_u = pad_plane(yuv.u(), vcw, vch, cw, ch);
    let src_v = pad_plane(yuv.v(), vcw, vch, cw, ch);

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
                Bpred::Gated => wb_sad > BPRED_GATE_SAD_PER_PIXEL * 256,
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
            let use_bpred = consider_bpred && bpred_sad + BPRED_SAD_PENALTY < wb_sad;

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
        out.extend_from_slice(&[len as u8, (len >> 8) as u8, (len >> 16) as u8]);
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
/// Returns [`Error::InvalidInput`] when `len` exceeds [`MAX_TOKEN_PARTITION_SIZE`], which the
/// prefix cannot describe. Split out from the writer so the ceiling is reachable from a test
/// without building a 16 MiB partition.
fn token_partition_size(len: usize) -> Result<u32> {
    match u32::try_from(len) {
        Ok(len) if len <= MAX_TOKEN_PARTITION_SIZE => Ok(len),
        _ => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: token partition exceeds the 3-byte size prefix",
        )),
    }
}

/// Splits the token-partition section (everything after the control partition) into `n` boolean
/// decoders (RFC 6386 §9.5): the first `n-1` partition sizes are 3-byte little-endian prefixes, the
/// last partition's size is the remainder.
fn split_token_partitions(data: &[u8], n: usize) -> Result<Vec<BoolDecoder<'_>>> {
    let sizes_len = (n - 1) * 3;
    if data.len() < sizes_len {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: token-partition sizes truncated",
        ));
    }
    let mut decoders = Vec::with_capacity(n);
    let mut offset = sizes_len;
    for i in 0..n {
        let size = if i < n - 1 {
            let s = &data[i * 3..i * 3 + 3];
            usize::from(s[0]) | (usize::from(s[1]) << 8) | (usize::from(s[2]) << 16)
        } else {
            data.len() - offset
        };
        let end = offset
            .checked_add(size)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "VP8: token partition exceeds frame")
            })?;
        decoders.push(BoolDecoder::new(&data[offset..end]));
        offset = end;
    }
    Ok(decoders)
}

/// Decodes a VP8 key-frame bitstream (the `VP8 ` chunk payload) into reconstructed planes.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for a malformed stream, or [`Error::Unsupported`] for an inter
/// frame or an undefined bitstream version (> 3).
pub fn decode_frame(data: &[u8]) -> Result<FrameBuffers> {
    let chunk = header::read_uncompressed_chunk(data)?;
    if chunk.width == 0 || chunk.height == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: zero frame dimension",
        ));
    }
    // RFC 6386 §9.1: the 3-bit version selects decoding profiles 0–3; 4–7 are undefined, and libwebp
    // rejects them, so we do too. Profiles 1–3 differ from 0 only in the inter-frame reconstruction
    // filter and a loop-filter hint; for intra key frames the explicit filter-type bit governs, so
    // 0–3 reconstruct identically here (pinned against libwebp in tests/oracle.rs).
    if chunk.version > 3 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "VP8: unsupported bitstream version",
        ));
    }
    let part0_end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
    if part0_end > data.len() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: first partition exceeds frame",
        ));
    }
    let mut modes = BoolDecoder::new(&data[UNCOMPRESSED_CHUNK_LEN..part0_end]);
    let (head, coeff_probs) = header::read_frame_header(&chunk, &mut modes);
    let seg_qf = segment_quant_factors(&head);
    let n = head.token_partitions as usize;
    let mut residuals = split_token_partitions(&data[part0_end..], n)?;
    let mut recon = FrameBuffers::new(u32::from(chunk.width), u32::from(chunk.height));

    let mut above = vec![EntropyCtx::default(); recon.mb_cols];
    let mut above_bmodes = vec![[B_DC_PRED; 4]; recon.mb_cols];
    let mut filter_interior = vec![false; recon.mb_cols * recon.mb_rows];
    let mut is_bpred_map = vec![false; recon.mb_cols * recon.mb_rows];
    let mut segment_map = vec![0usize; recon.mb_cols * recon.mb_rows];
    for mb_y in 0..recon.mb_rows {
        let mut left = EntropyCtx::default();
        let mut left_bmodes = [B_DC_PRED; 4];
        for mb_x in 0..recon.mb_cols {
            let segment = if head.segmentation.update_map {
                modes.get_tree(MB_SEGMENT_TREE, &head.segmentation.tree_probs)
            } else {
                0
            };
            segment_map[mb_y * recon.mb_cols + mb_x] = segment;
            let qf = seg_qf[segment];
            let skip = head.mb_no_skip_coeff && modes.get_bool(head.prob_skip_false);
            let y_mode = modes.get_tree(prediction::KF_YMODE_TREE, &prediction::KF_YMODE_PROB);
            let is_bpred = y_mode == B_PRED;
            let sub_modes = if is_bpred {
                read_bmodes(&mut modes, &above_bmodes[mb_x], &left_bmodes)
            } else {
                [B_DC_PRED; 16]
            };
            let uv_mode = modes.get_tree(prediction::KF_UV_MODE_TREE, &prediction::KF_UV_MODE_PROB);
            let u_pred = predict_chroma(&recon.u, recon.c_stride(), mb_x, mb_y, uv_mode);
            let v_pred = predict_chroma(&recon.v, recon.c_stride(), mb_x, mb_y, uv_mode);
            let cstride = recon.c_stride();

            // A skipped macroblock has no coefficients: its residual is zero (the reconstruction is the
            // prediction) and no tokens are read.
            let mut levels = MbLevels::default();
            if is_bpred {
                let above_right = above_right_source(&recon, mb_x, mb_y);
                if skip {
                    reconstruct_bpred_zero(&mut recon, mb_x, mb_y, &sub_modes, &above_right);
                } else {
                    decode_bpred_luma(
                        &mut recon,
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                        mb_x,
                        mb_y,
                        &qf,
                        &sub_modes,
                        &above_right,
                    );
                    decode_chroma_tokens(
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                        &mut levels,
                    );
                }
                reconstruct_chroma(&mut recon.u, cstride, mb_x, mb_y, &u_pred, &levels.u, &qf);
                reconstruct_chroma(&mut recon.v, cstride, mb_x, mb_y, &v_pred, &levels.v, &qf);
                filter_interior[mb_y * recon.mb_cols + mb_x] = true; // B_PRED always filters interiors
                is_bpred_map[mb_y * recon.mb_cols + mb_x] = true;
            } else {
                let y_pred = predict_luma(&recon, mb_x, mb_y, y_mode);
                if !skip {
                    levels = decode_mb_tokens(
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                    );
                }
                reconstruct_luma(&mut recon, mb_x, mb_y, &y_pred, &levels, &qf);
                reconstruct_chroma(&mut recon.u, cstride, mb_x, mb_y, &u_pred, &levels.u, &qf);
                reconstruct_chroma(&mut recon.v, cstride, mb_x, mb_y, &v_pred, &levels.v, &qf);
                filter_interior[mb_y * recon.mb_cols + mb_x] = mb_has_coeffs(&levels);
            }
            if skip {
                clear_mb_context(&mut above[mb_x], &mut left, is_bpred);
            }

            (above_bmodes[mb_x], left_bmodes) = bmode_propagation(is_bpred, y_mode, &sub_modes);
        }
    }

    apply_loop_filter(
        &mut recon,
        &head.loop_filter,
        &head.segmentation,
        &segment_map,
        &filter_interior,
        &is_bpred_map,
    );
    Ok(recon)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `Yuv420` from a deterministic synthetic pattern.
    fn pattern(width: u32, height: u32) -> Yuv420 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (
            Yuv420::chroma_width(width) as usize,
            Yuv420::chroma_height(height) as usize,
        );
        let y = (0..w * h)
            .map(|i| ((i * 7 + i / w * 13) & 0xff) as u8)
            .collect();
        let u = (0..cw * ch).map(|i| ((i * 5 + 64) & 0xff) as u8).collect();
        let v = (0..cw * ch)
            .map(|i| ((i * 11 + 128) & 0xff) as u8)
            .collect();
        Yuv420::new(width, height, y, u, v).unwrap()
    }

    /// Builds B_PRED-favorable content: each 4×4 region carries a different gradient direction, so a
    /// single whole-block mode predicts the macroblock poorly but per-subblock modes do not.
    fn detailed(width: u32, height: u32) -> Yuv420 {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (
            Yuv420::chroma_width(width) as usize,
            Yuv420::chroma_height(height) as usize,
        );
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
        Yuv420::new(width, height, y, u, v).unwrap()
    }

    /// Counts macroblocks coded as `B_PRED` by re-reading partition 0, to confirm the path is
    /// genuinely exercised (not merely available).
    /// Re-reads partition 0 (modes) and returns `(B_PRED macroblocks, skipped macroblocks)`, to
    /// confirm those paths are genuinely exercised.
    fn mode_stats(data: &[u8]) -> (usize, usize) {
        let chunk = header::read_uncompressed_chunk(data).unwrap();
        let part0_end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
        let mut modes = BoolDecoder::new(&data[UNCOMPRESSED_CHUNK_LEN..part0_end]);
        let (head, _) = header::read_frame_header(&chunk, &mut modes);
        let mb_cols = (chunk.width as usize).div_ceil(16);
        let mb_rows = (chunk.height as usize).div_ceil(16);
        let mut above_bmodes = vec![[B_DC_PRED; 4]; mb_cols];
        let (mut bpred, mut skipped) = (0, 0);
        for _ in 0..mb_rows {
            let mut left_bmodes = [B_DC_PRED; 4];
            for mb_x in 0..mb_cols {
                if head.segmentation.update_map {
                    let _ = modes.get_tree(MB_SEGMENT_TREE, &head.segmentation.tree_probs);
                }
                if head.mb_no_skip_coeff && modes.get_bool(head.prob_skip_false) {
                    skipped += 1;
                }
                let y_mode = modes.get_tree(prediction::KF_YMODE_TREE, &prediction::KF_YMODE_PROB);
                let is_bpred = y_mode == B_PRED;
                let sub_modes = if is_bpred {
                    bpred += 1;
                    read_bmodes(&mut modes, &above_bmodes[mb_x], &left_bmodes)
                } else {
                    [B_DC_PRED; 16]
                };
                let _ = modes.get_tree(prediction::KF_UV_MODE_TREE, &prediction::KF_UV_MODE_PROB);
                (above_bmodes[mb_x], left_bmodes) = bmode_propagation(is_bpred, y_mode, &sub_modes);
            }
        }
        (bpred, skipped)
    }

    #[test]
    fn bpred_is_exercised_and_bit_exact() {
        let yuv = detailed(48, 48);
        let (bitstream, recon) =
            encode_frame(&yuv, 8).expect("fixture fits the partition-size fields");
        assert!(
            mode_stats(&bitstream).0 > 0,
            "detailed content should select B_PRED for some macroblocks"
        );
        let decoded = decode_frame(&bitstream).expect("decode");
        let (enc, dec) = (recon.to_yuv420(), decoded.to_yuv420());
        assert_eq!(enc.y(), dec.y(), "B_PRED luma mismatch");
        assert_eq!(enc.u(), dec.u(), "B_PRED u mismatch");
        assert_eq!(enc.v(), dec.v(), "B_PRED v mismatch");
    }

    #[test]
    fn mb_skip_is_exercised_and_bit_exact() {
        // A flat image predicts to 128 with a zero residual, so every macroblock is skipped; the
        // decode must reproduce it from the skip flags alone.
        let (w, h) = (48u32, 48u32);
        let (cw, ch) = (
            Yuv420::chroma_width(w) as usize,
            Yuv420::chroma_height(h) as usize,
        );
        let yuv = Yuv420::new(
            w,
            h,
            vec![128u8; (w * h) as usize],
            vec![128u8; cw * ch],
            vec![128u8; cw * ch],
        )
        .unwrap();
        let (bits, recon) = encode_frame(&yuv, 60).expect("fixture fits the partition-size fields");
        assert!(
            mode_stats(&bits).1 > 0,
            "flat content should skip macroblocks"
        );
        let dec = decode_frame(&bits).expect("decode");
        assert_eq!(recon.to_yuv420().y(), dec.to_yuv420().y());
        assert_eq!(recon.to_yuv420().u(), dec.to_yuv420().u());
        assert_eq!(recon.to_yuv420().v(), dec.to_yuv420().v());
    }

    /// Tier-2: the encoder's reconstruction must equal the native decoder's output, bit-for-bit.
    fn assert_encoder_recon_matches_decoder(width: u32, height: u32, q: u8) {
        let yuv = pattern(width, height);
        let (bitstream, recon) =
            encode_frame(&yuv, q).expect("fixture fits the partition-size fields");
        let decoded = decode_frame(&bitstream).expect("decode");
        let enc = recon.to_yuv420();
        let dec = decoded.to_yuv420();
        assert_eq!(enc.y(), dec.y(), "luma mismatch at {width}x{height} q{q}");
        assert_eq!(enc.u(), dec.u(), "u mismatch at {width}x{height} q{q}");
        assert_eq!(enc.v(), dec.v(), "v mismatch at {width}x{height} q{q}");
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
                let (enc, dec) = (recon.to_yuv420(), dec.to_yuv420());
                assert_eq!(enc.y(), dec.y(), "luma simple={simple} q{q}");
                assert_eq!(enc.u(), dec.u(), "u simple={simple} q{q}");
                assert_eq!(enc.v(), dec.v(), "v simple={simple} q{q}");
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
            let (enc, dec) = (recon.to_yuv420(), dec.to_yuv420());
            assert_eq!(enc.y(), dec.y(), "luma q{q}");
            assert_eq!(enc.u(), dec.u(), "u q{q}");
            assert_eq!(enc.v(), dec.v(), "v q{q}");
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
            let (enc, dec) = (recon.to_yuv420(), dec.to_yuv420());
            assert_eq!(enc.y(), dec.y(), "luma p{partitions}");
            assert_eq!(enc.u(), dec.u(), "u p{partitions}");
            assert_eq!(enc.v(), dec.v(), "v p{partitions}");
        }
    }

    #[test]
    fn loop_filter_deltas_reconstruct_bit_exact() {
        // mb_lf_adjustments shift each macroblock's filter level; the encoder writes them into the
        // header and applies them to its own reconstruction, so the native decoder must reproduce the
        // same deblocked planes. detailed() forces B_PRED macroblocks, so the mode[0] (B_PRED) delta
        // is genuinely exercised, not just ref_frame[0]. (The cross-check that this matches libwebp's
        // own delta handling is the tier-3 oracle in tests/oracle.rs.)
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
                    let (enc, dec) = (recon.to_yuv420(), dec.to_yuv420());
                    assert_eq!(enc.y(), dec.y(), "luma ref={ref_d} mode={mode_d} q{q}");
                    assert_eq!(enc.u(), dec.u(), "u ref={ref_d} mode={mode_d} q{q}");
                    assert_eq!(enc.v(), dec.v(), "v ref={ref_d} mode={mode_d} q{q}");
                }
            }
        }
    }

    #[test]
    fn decode_accepts_profiles_0_to_3_and_rejects_higher() {
        // RFC 6386 §9.1: the frame-tag version selects profiles 0–3 (4–7 undefined). The version sits
        // in bits 1–3 of byte 0; patching it must not change which (valid) frame decodes.
        let yuv = pattern(32, 32);
        let (bits, _) = encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        let patch_version = |v: u8| {
            let mut p = bits.clone();
            p[0] = (p[0] & !0b1110) | (v << 1);
            p
        };
        for v in 0u8..=3 {
            assert!(
                decode_frame(&patch_version(v)).is_ok(),
                "profile {v} must decode"
            );
        }
        for v in 4u8..=7 {
            assert!(
                matches!(
                    decode_frame(&patch_version(v)),
                    Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
                ),
                "profile {v} must be rejected"
            );
        }
    }

    #[test]
    fn decode_rejects_truncated_first_partition() {
        let yuv = pattern(16, 16);
        let (mut bitstream, _) =
            encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        bitstream.truncate(UNCOMPRESSED_CHUNK_LEN + 1);
        let _ = decode_frame(&bitstream);
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
    fn segment_filter_level_applies_signed_delta() {
        // Delta mode: the per-segment level is `base + filter_strength`, clamped to [0, 63]. A
        // negative delta must lower the level (pins the `+`, which flipped to `-` would raise it).
        let seg = Segmentation {
            enabled: true,
            abs_delta: false,
            filter_strength: [10, -10, 0, 40],
            ..Default::default()
        };
        assert_eq!(segment_filter_level(30, &seg, 0), 40);
        assert_eq!(segment_filter_level(30, &seg, 1), 20);
        assert_eq!(segment_filter_level(30, &seg, 3), 63);
        // Absolute mode and the disabled fast path.
        let abs = Segmentation {
            enabled: true,
            abs_delta: true,
            filter_strength: [25, 0, 0, 0],
            ..Default::default()
        };
        assert_eq!(segment_filter_level(30, &abs, 0), 25);
        assert_eq!(segment_filter_level(30, &Segmentation::default(), 0), 30);
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
    fn decode_rejects_zero_dimension() {
        let yuv = pattern(16, 16);
        let (mut bits, _) = encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        // Clear the 14-bit width field (low 6 bits live in byte 7) while leaving height non-zero, so
        // only the `width == 0` half of the guard fires: `||` flipped to `&&` would let it through.
        bits[6] = 0;
        bits[7] &= 0xC0;
        assert!(matches!(
            decode_frame(&bits),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn split_token_partitions_size_table_boundary() {
        // Exactly the size-table length (3 bytes for n = 2) with a zero first size is valid: two
        // empty partitions. `<` widened to `<=` would wrongly reject it as truncated.
        assert!(split_token_partitions(&[0, 0, 0], 2).is_ok());
        // One byte short of the size table is truncated and must be rejected; `<` narrowed to `==`
        // would skip the bounds check.
        assert!(split_token_partitions(&[0, 0], 2).is_err());
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
        let (cw, ch) = (
            Yuv420::chroma_width(w) as usize,
            Yuv420::chroma_height(h) as usize,
        );
        let yuv = Yuv420::new(w, h, y, vec![128; cw * ch], vec![128; cw * ch]).unwrap();
        let (bits, recon) = encode_frame_filtered(&yuv, 40, EncodeOptions::default())
            .expect("fixture fits the partition-size fields");
        let dec = decode_frame(&bits).expect("decode");
        assert_eq!(recon.to_yuv420().y(), dec.to_yuv420().y());
    }

    #[test]
    fn decode_accepts_exact_fit_with_empty_token_partition() {
        // A flat image quantizes to all-skip macroblocks, so the single token partition carries no
        // tokens. Truncating the stream to exactly the end of the first partition leaves zero
        // token-partition bytes — part0_end == data.len(). The `part0_end > data.len()` guard must
        // accept this exact fit; `>=` would reject the valid (if minimal) stream.
        let (w, h) = (16u32, 16u32);
        let (cw, ch) = (
            Yuv420::chroma_width(w) as usize,
            Yuv420::chroma_height(h) as usize,
        );
        let yuv = Yuv420::new(
            w,
            h,
            vec![128; (w * h) as usize],
            vec![128; cw * ch],
            vec![128; cw * ch],
        )
        .unwrap();
        let (bits, _) = encode_frame(&yuv, 127).expect("fixture fits the partition-size fields"); // coarsest quantizer → every macroblock skips
        let chunk = header::read_uncompressed_chunk(&bits).expect("chunk");
        let end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
        assert!(
            decode_frame(&bits[..end]).is_ok(),
            "a stream ending exactly at the first partition (no token bytes) must decode"
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
