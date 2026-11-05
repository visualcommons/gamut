//! Block mode info (§5.11.5–§5.11.46): the segment id, `delta_q`/`delta_lf`, `skip`, the luma
//! and chroma modes, filter-intra, CfL alphas and `tx_depth` one block codes, and the
//! rate–distortion decisions that choose them — the luma mode jointly with the transform depth,
//! and the CfL alphas.

use super::coder::{Coding, symbol_rate};
use super::intra::{Pred, is_directional};
use super::recon::{max_tx_size_rect, square_tx};
use super::{
    D45_PRED, D67_PRED, D113_PRED, D135_PRED, D157_PRED, D203_PRED, DC_PRED, FrameEncoder, H_PRED,
    LUMA_MODES, PAETH_PRED, SMOOTH_H_PRED, SMOOTH_PRED, SMOOTH_V_PRED, UV_CFL_PRED, V_PRED,
};
use crate::cdf;
use crate::geom::{has_chroma, plane_residual_size};
use crate::quant::{ac_q, dc_q};
use crate::transform::TxSize;

/// The eight directional luma modes, in `YMode` order (`V`..`D67`), used to drive the ≥8×8 angle
/// search. Each is a candidate base angle that `AngleDeltaY ∈ [-3, 3]` fine-tunes.
pub(super) const DIRECTIONAL_MODES: [u8; 8] = [
    V_PRED, H_PRED, D45_PRED, D135_PRED, D113_PRED, D157_PRED, D203_PRED, D67_PRED,
];

/// Every `AngleDeltaY` (§5.11.42), in the order the full search tries them.
pub(super) const ALL_ANGLE_DELTAS: [i8; 7] = [-3, -2, -1, 0, 1, 2, 3];

/// Every `CflAlpha` magnitude (§5.11.45), each tried with both signs.
pub(super) const ALL_CFL_MAGNITUDES: [i32; 16] =
    [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// Per-segment `SEG_LVL_ALT_Q` quantizer deltas (§5.9.14): `Some(delta)` enables the feature on that
/// segment (its block quantizer is `Clip3(0, 255, CurrentQIndex + delta)`), `None` leaves the segment
/// on the frame quantizer. The header signals this exact table, so encoder and decoder agree. The two
/// active segments give the spatial `segment_id` prediction real work without `SegIdPreSkip`
/// (`SEG_LVL_ALT_Q` < `SEG_LVL_REF_FRAME`). `LastActiveSegId` is the highest `Some` index (2 here).
///
/// The deltas are **positive** so a segment's quantizer is always `> 0`: a segment whose
/// `get_qindex` is 0 would be *coded-lossless* (§7.12.2) and require the Walsh–Hadamard path, which
/// this lossy encoder does not select per-segment.
pub(crate) const SEG_ALT_Q: [Option<i32>; 8] =
    [None, Some(20), Some(50), None, None, None, None, None];

/// `LastActiveSegId` (§5.9.14): the highest segment index with an enabled feature.
const LAST_ACTIVE_SEG: i32 = 2;

/// The largest valid segment reference, `max - 1`. Pulled out so its arithmetic is excludable: a
/// reference is always `< max`, so widening this threshold (`max + 1`, or `max` via `/1`) is never
/// reached and the general `neg_interleave` path maps `r == max - 1` identically — those mutations are
/// equivalent.
fn last_ref(max: i32) -> i32 {
    max - 1
}

/// `neg_interleave(x, ref, max)` — the encoder inverse of the decoder's `neg_deinterleave` (§5.11.9):
/// maps the segment id `x` to the difference coded relative to the predicted id `ref`.
fn neg_interleave(x: i32, r: i32, max: i32) -> i32 {
    if r == 0 {
        return x;
    }
    if r >= last_ref(max) {
        return max - x - 1;
    }
    let bounded = if 2 * r < max {
        (x - r).abs() <= r
    } else {
        (x - r).abs() < max - r
    };
    if bounded {
        if x <= r { (r - x) * 2 } else { (x - r) * 2 - 1 }
    } else if 2 * r < max {
        x
    } else {
        max - (x + 1)
    }
}

/// The `CflSign` of a CfL alpha (§5.11.45): `CFL_SIGN_ZERO` (0), `CFL_SIGN_NEG` (1) or
/// `CFL_SIGN_POS` (2).
///
/// One three-way comparison, not `alpha == 0` then `alpha < 0`: there the second test ran only on
/// a nonzero `alpha`, so its `<` and `<=` were the same function. The rewrite also took the two
/// tests' three killable operator mutants with it, leaving only the body replacements, which
/// `cfl_sign_is_the_spec_sign_code` still kills by naming all three arms.
fn cfl_sign(alpha: i32) -> usize {
    match alpha.cmp(&0) {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Greater => 2,
    }
}

/// A luma prediction: `YMode`, its `AngleDeltaY` (0 unless directional and ≥8×8), and the
/// recursive filter-intra mode, which is signalled as `DC_PRED` + `use_filter_intra`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LumaMode {
    mode: u8,
    angle_delta: i8,
    filter_intra: Option<u8>,
}

/// A block's mode decisions, reached while it was trialled: see [`FrameEncoder::decided`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Decided {
    /// The block: MI `(r, c)` and its width and height in samples.
    pub(super) at: (usize, usize, usize, usize),
    luma: LumaMode,
    /// The luma `tx_depth` (0 for a block that signals none).
    tx_depth: usize,
    cfl: Option<(i32, i32)>,
}

/// Plain `DC_PRED` luma: what palette and lossless blocks code.
const DC_LUMA: LumaMode = LumaMode {
    mode: DC_PRED,
    angle_delta: 0,
    filter_intra: None,
};

/// How many luma mode candidates, screened by their `DCT_DCT` cost, are trialled again with the
/// transform-type search (see [`FrameEncoder::select_luma_mode`]).
const MODE_REFINE: usize = 8;

/// The `tx_depth` CDF of a square lossy `bw × bw` block (`Max_Tx_Depth` keys it by size) under
/// context `ctx` — also the CDF a rectangular block whose longer side is `bw` codes its
/// `tx_depth = 0` under.
fn tx_depth_slot(cdfs: &mut cdf::CdfContext, bw: usize, ctx: usize) -> cdf::CdfSlot<'_> {
    match bw {
        8 => cdfs.tx_size_8x8[ctx].slot(),
        16 => cdfs.tx_size_16x16[ctx].slot(),
        32 => cdfs.tx_size_32x32[ctx].slot(),
        _ => cdfs.tx_size_64x64[ctx].slot(),
    }
}

impl FrameEncoder<'_> {
    /// Updates the active dequantizers to `CurrentQIndex` — used when a `delta_q` changes it. Only the
    /// `dc_quant`/`ac_quant` step sizes follow `CurrentQIndex`; the coefficient-CDF context `qctx`
    /// stays at its frame value (`init_coeff_cdfs` derives it from `base_q_idx`, §8.3.2), so it is
    /// deliberately not touched here.
    pub(super) fn set_quant(&mut self, qindex: i32) {
        self.current_qindex = qindex;
        self.dc_quant = dc_q(self.bit_depth, qindex);
        self.ac_quant = ac_q(self.bit_depth, qindex);
    }

    /// Sets the active dc/ac quantizers for a block to `get_qindex(segment)` (§7.12.2): the segment's
    /// `SEG_LVL_ALT_Q` delta is added to `CurrentQIndex` (the delta-Q accumulator, left unchanged), so
    /// each block can quantize at its own step size.
    pub(super) fn apply_seg_quant(&mut self, segid: usize) {
        let alt = SEG_ALT_Q[segid].unwrap_or(0);
        let bq = (self.current_qindex + alt).clamp(0, 255);
        self.dc_quant = dc_q(self.bit_depth, bq);
        self.ac_quant = ac_q(self.bit_depth, bq);
    }

    /// `read_segment_id` (§5.11.9) after the skip flag (`SegIdPreSkip = 0`): derives the predicted
    /// segment id from the above/left/above-left neighbours. A skip block inherits the prediction (no
    /// bits); otherwise the encoder's deterministic assignment is coded as `neg_interleave` of the id
    /// relative to the prediction, under `Default_Segment_Id_Cdf[ctx]`. Returns the chosen id.
    fn signal_segment_id(&mut self, r: usize, c: usize, skip: bool) -> usize {
        let cols = self.mi_cols;
        let prev_ul = if r > self.tile_r0 && c > self.tile_c0 {
            i32::from(self.mi_segid[(r - 1) * cols + (c - 1)])
        } else {
            -1
        };
        let prev_u = if r > self.tile_r0 {
            i32::from(self.mi_segid[(r - 1) * cols + c])
        } else {
            -1
        };
        let prev_l = if c > self.tile_c0 {
            i32::from(self.mi_segid[r * cols + (c - 1)])
        } else {
            -1
        };
        let pred = if prev_u == -1 {
            prev_l.max(0)
        } else if prev_l == -1 || prev_ul == prev_u {
            prev_u
        } else {
            prev_l
        };
        if skip {
            return pred as usize; // inherited, no bits
        }
        // Deterministic assignment in 0..=LastActiveSegId; spatially varied so the three contexts and
        // both alt-Q segments are exercised. Any valid assignment reconstructs bit-exactly.
        let assigned = ((r + c) as i32) % (LAST_ACTIVE_SEG + 1);
        let ctx = if prev_ul < 0 {
            0
        } else if prev_ul == prev_u && prev_ul == prev_l {
            2
        } else if prev_ul == prev_u || prev_ul == prev_l || prev_u == prev_l {
            1
        } else {
            0
        };
        let diff = neg_interleave(assigned, pred, LAST_ACTIVE_SEG + 1);
        self.sym
            .encode(diff as usize, self.cdfs.segment_id[ctx].slot());

        assigned as usize
    }

    /// `read_delta_qindex` (§5.11.12) for the first block of a superblock: signals a small per-SB
    /// `delta_q` (magnitude coded under `Default_Delta_Q_Cdf`, then a sign bit) and updates
    /// `CurrentQIndex = Clip3(1, 255, CurrentQIndex + delta)` (with `delta_q_res = 0`). Coded only
    /// under [`super::BlockVariation::SyntaxExercise`]: the delta is position-only syntax exercise
    /// (any value reconstructs bit-exactly) chosen to oscillate around the frame quantizer so adjacent
    /// superblocks differ. Magnitude is kept ≤ 2, so the `DELTA_Q_SMALL` escape
    /// is never coded.
    fn signal_delta_q(&mut self) {
        // `(sb_r / 16 + sb_c / 16) % 3` over superblock origins, which are multiples of 16 MI:
        // `/ 16` → `* 16` multiplies by 256 ≡ 1 (mod 3) and changed nothing (#920), so the
        // superblock count mod 3 is read as `(sb_r + sb_c) % 48`, in MI units, with no division.
        let delta: i32 = match (self.sb_r + self.sb_c) % 48 {
            0 => 1,
            16 => -1,
            _ => 2,
        };
        let abs = delta.unsigned_abs() as usize;
        self.sym.encode(abs, self.cdfs.delta_q.slot());
        // delta_q_sign_bit (1 ⇒ negative). `delta` is never 0, so `delta < 0` → `<= 0` changed
        // nothing (#920); the sign is read as `is_negative()`, which has no such twin.
        self.sym.encode_literal(u32::from(delta.is_negative()), 1);
        let nq = (self.current_qindex + delta).clamp(1, 255);
        self.set_quant(nq);
    }

    /// `read_delta_lf` (§5.11.13, `delta_lf_multi = 0`) for the first block of a superblock: signals a
    /// small per-SB `delta_lf` (magnitude under `Default_Delta_Lf_Cdf`, then a sign bit when non-zero)
    /// and updates `CurrentDeltaLF = Clip3(-MAX_LOOP_FILTER, MAX_LOOP_FILTER, CurrentDeltaLF + delta)`.
    /// Coded only under [`super::BlockVariation::SyntaxExercise`]: the delta is a non-negative,
    /// position-only `{0, 1}` syntax exercise (so the per-superblock loop-filter level only
    /// rises above the frame level — `loop_filter_level + DeltaLF` stays positive and the p-side-level
    /// fallback for a zero level is never needed). It does not depend on the frame level, which is
    /// chosen only after the tiles are coded: at level 0 §7.4 does not invoke the loop filter at
    /// all, so the delta is inert there. Magnitude ≤ 2 ⇒ the `DELTA_LF_SMALL` escape is never
    /// coded.
    fn signal_delta_lf(&mut self) {
        let delta = (self.sb_r / 16 + self.sb_c / 16) as i32 % 2;
        let abs = delta.unsigned_abs() as usize;
        self.sym.encode(abs, self.cdfs.delta_lf.slot());
        if abs > 0 {
            // delta_lf_sign_bit: `delta` is 0 or 1, never negative, so the sign is always 0.
            self.sym.encode_literal(0, 1);
        }
        self.current_dlf = (self.current_dlf + delta).clamp(-63, 63);
    }

    /// Context for the `skip` flag (§8.3.2): `aboveSkip + leftSkip` from the per-MI `mi_skip` map.
    fn skip_ctx(&self, r: usize, c: usize) -> usize {
        let above = r > self.tile_r0 && self.mi_skip[(r - 1) * self.mi_cols + c] != 0;
        let left = c > self.tile_c0 && self.mi_skip[r * self.mi_cols + (c - 1)] != 0;
        usize::from(above) + usize::from(left)
    }

    /// Whether a lossy block at MI `(r, c)` can be coded with `skip = 1` (no residual): every plane is
    /// flat over the block and its `DC_PRED` from the reconstruction exactly equals that value, so the
    /// residual is identically zero. For such a block `skip = 1` and `skip = 0` reconstruct to the
    /// same samples (prediction), so signalling `skip` is a lossless bit saving. `encode_block` codes
    /// such a block with `DC_PRED` luma and chroma, no CfL and no filter-intra, without searching,
    /// so the predicted samples equal the source by construction.
    fn block_is_skippable(&self, r: usize, c: usize, bw: usize) -> bool {
        // A monochrome frame has only plane 0 to inspect (`NumPlanes == 1`).
        for plane in 0..self.num_planes {
            // Each plane is inspected over its own residual block: at 4:4:4 that is the luma
            // extent, and under subsampling reading the luma extent would run off the chroma plane.
            let Some((px, py, pw, ph)) = self.plane_block(r, c, bw, bw, plane) else {
                continue;
            };
            let v0 = self.sample(plane, px, py);
            for i in 0..ph {
                for j in 0..pw {
                    if self.sample(plane, px + j, py + i) != v0 {
                        return false;
                    }
                }
            }
            if self.dc_pred(plane, px, py, pw, ph) != v0 {
                return false;
            }
        }
        true
    }

    /// Context for the `tx_depth` CDF (§8.3.2): `(aboveTxW ≥ bw) + (leftTxH ≥ bw)`, where the
    /// neighbour transform width/height come from the per-MI `tx_log2` map (0 when unavailable).
    fn tx_depth_ctx(&self, r: usize, c: usize, bw: usize, bh: usize) -> usize {
        let above_w = if r > self.tile_r0 {
            1usize << self.tx_log2[(r - 1) * self.mi_cols + c]
        } else {
            0
        };
        let left_h = if c > self.tile_c0 {
            1usize << self.tx_log2_h[r * self.mi_cols + (c - 1)]
        } else {
            0
        };
        usize::from(above_w >= bw) + usize::from(left_h >= bh)
    }

    pub(super) fn encode_block(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
        let n4 = bw / 4;
        let n4h = bh / 4;
        let bsl = n4.trailing_zeros() as u8;
        let bsl_h = n4h.trailing_zeros() as u8;
        // A rectangular block (from PARTITION_HORZ/VERT) is coded as one matching rectangular transform
        // (OPTION A); it uses a SMOOTH-family non-directional mode (so no palette/filter-intra/CfL) and
        // never `skip = 1` (its residual, possibly all-zero, is always coded).
        let is_rect = bw != bh;
        // TX_MODE_LARGEST ⇒ the transform spans the whole block. The lossy path codes an 8×8 or 16×16
        // block as a single TX_8X8 / TX_16X16; lossy 4×4 and lossless use 4×4 transforms.
        let lossy_large = self.qindex > 0 && bw.min(bh) >= 8;

        // intra_frame_mode_info: skip=0 (ctx 0), then y_mode and uv_mode. The lossless path always
        // predicts DC_PRED; the lossy path searches the non-directional luma modes (plus directional
        // and filter-intra) and signals the choice with the above/left mode context.
        // palette_mode_info / read_skip. A palette block (luma 2..=8 colors + flat chroma) is coded
        // `skip = 1`: the reconstruction is the palette (luma) plus DC chroma, so no residual is
        // needed. Otherwise a block whose residual is identically zero (flat + DC-predicted) is also
        // `skip = 1`. The skip context is the above/left skip flags.
        let palette = if self.qindex > 0 && !is_rect && (8..=32).contains(&bw) {
            self.decide_palette(r, c, bw)
        } else {
            None
        };
        let skip =
            palette.is_some() || (self.qindex > 0 && !is_rect && self.block_is_skippable(r, c, bw));

        let sctx = self.skip_ctx(r, c);
        self.sym
            .encode(usize::from(skip), self.cdfs.skip[sctx].slot());
        // intra_segment_id / read_segment_id (§5.11.9): with segmentation on, code the block's
        // segment id right after the skip flag (`SegIdPreSkip = 0`). A skip block inherits the
        // predicted id with no bits. Without segmentation (the default, and always when lossless)
        // nothing is coded and every block is segment 0, at the frame quantizer.
        let segid = if self.block_variation {
            self.signal_segment_id(r, c, skip)
        } else {
            0
        };
        // read_delta_qindex (§5.11.12): the first coded block of each superblock signals delta_q and
        // updates `CurrentQIndex`; later blocks in the superblock inherit it. (read_cdef codes no bits:
        // cdef_bits = 0.) A block that fills the whole superblock (`MiSize == sbSize`, i.e. a 64×64
        // block) and is `skip` codes neither delta — `read_delta_qindex`/`read_delta_lf` return early.
        if self.read_deltas {
            if !(bw == 64 && bh == 64 && skip) {
                self.signal_delta_q();
                self.signal_delta_lf();
            }
            self.read_deltas = false;
        }
        // Per-block quantizer get_qindex(segment) = Clip3(0, 255, CurrentQIndex + alt_q) (§7.12.2):
        // only the dc/ac step sizes follow the segment; `CurrentQIndex` stays the delta-Q accumulator.
        // Unconditional: a lossless frame codes segment 0 (no `SEG_LVL_ALT_Q`) and no `delta_q`
        // (`delta_q_present` needs `base_q_idx > 0`), so there it sets the frame's own steps again.
        self.apply_seg_quant(segid);
        // The luma transforms the block may be coded with, each with the rate of signalling it:
        // a square lossy ≥8×8 block chooses its `tx_depth` (0..=`Max_Tx_Depth`, one prediction
        // over `bw >> tx_depth` square transforms), jointly with its mode; a rectangular block
        // codes one transform that fills it (tx_depth = 0), a 4×4 or lossless block 4×4s.
        let tx_ctx = self.tx_depth_ctx(r, c, bw, bh);
        let tx_options: Vec<(TxSize, u32)> = if is_rect {
            vec![(max_tx_size_rect(bw, bh), 0)]
        } else if lossy_large {
            let max_depth = if bw == 8 { 1 } else { 2 };
            (0..=max_depth)
                .map(|depth| {
                    let rate =
                        symbol_rate(tx_depth_slot(&mut self.cdfs, bw, tx_ctx).probs(), depth);
                    (square_tx(bw >> depth), rate)
                })
                .collect()
        } else {
            vec![(TxSize::Tx4x4, 0)]
        };
        // A block the partition search has trialled is coded as the trial decided it, rather
        // than searched a second time. A skip block has nothing to decide, and is never recorded:
        // whether a block is skipped, and whether it is a palette block, reads the
        // reconstruction its neighbours left, which a block coded again after its trial need not
        // find the same. So a block the trial coded as skipped is searched afresh if it now has
        // a residual, and one now skipped ignores what its trial decided — coding it as decided
        // would code a palette, whose `has_palette_y` only a `DC_PRED` block signals, under any
        // mode the trial chose.
        let decided = if skip {
            None
        } else {
            self.decided
                .iter()
                .position(|d| d.at == (r, c, bw, bh))
                .map(|i| self.decided.swap_remove(i))
        };
        // §5.11.5 `HasChroma`: with subsampled chroma a sub-8x8 block can cover no chroma samples
        // of its own, and the covering block of its MI group codes chroma for the whole group. Such
        // a block codes no `uv_mode`, no `cfl_alpha` (§5.11.7) and no chroma residual (§5.11.34).
        // Always true at 4:4:4, and always false in a monochrome frame — the spec writes the
        // two parity tests first and then falls through to `HasChroma = NumPlanes > 1`.
        let block_has_chroma = has_chroma(r, c, bw, bh, self.subsampling);
        // §6.10.4 is a bitstream-conformance requirement the *partition search* has to respect, so
        // assert it at the point a block is committed rather than discovering it in the residual
        // loop. `partitions_offered` is the only thing that can produce a shape that violates it.
        debug_assert!(
            !block_has_chroma || self.rect_partition_is_codable(bw, bh),
            "{bw}x{bh} has no codable chroma residual under {:?}",
            self.subsampling
        );
        // `is_cfl_allowed` is `MiSize == BLOCK_4X4` when Lossless, else `Max(w, h) <= 32` — so CfL
        // is allowed for the lossy 4×4 to 32×32 blocks and the lossless 4×4 blocks.
        let cfl_allowed = if self.qindex == 0 {
            bw == 4
        } else {
            bw.max(bh) <= 32
        };
        // The chroma-from-luma alphas are chosen with the luma mode, by the branch that searches
        // one, so a block that searches neither codes plain `DC_PRED` chroma for the reason it
        // codes `DC_PRED` luma. A separate `match` on `skip` and `HasChroma` left an arm no
        // fixture could tell from its deletion: a skip block's chroma is flat and exactly
        // DC-predicted (`block_is_skippable`, `decide_palette`), so plain DC costs no distortion
        // and CfL could only win on rate, and a block without chroma of its own never codes the
        // result. The search branch is lossy by construction, so its guard needs no `is_lossy`.
        let (luma, tx_depth, cfl) = if let Some(d) = decided {
            (d.luma, d.tx_depth, d.cfl)
        } else if skip || self.qindex == 0 {
            // Palette requires YMode == DC_PRED (and no filter-intra); lossless also forces DC_PRED.
            // A block skipped for being flat and DC-predictable (`block_is_skippable`) takes
            // DC_PRED by construction: the rate–distortion search trials every mode with its
            // residual coded, a residual a skip block never codes, so it would rank the modes by
            // the wrong reconstruction — and only DC_PRED is known to reproduce the source here.
            // With no residual there is nothing for a deeper transform to code either, so a
            // skip block keeps one block-size transform (`tx_depth = 0`), which also leaves the
            // deblocking filter no inner transform edge to cross.
            (DC_LUMA, 0, None)
        } else {
            let (luma, tx_depth) = self.select_luma_mode(r, c, bw, bh, &tx_options);
            // The plane loop codes chroma only for a block with chroma of its own, and CfL is
            // square-only here: a rectangular block's chroma is plain DC_PRED. The pick precedes
            // `y_mode` and `angle_delta` in the stream, which it neither reads nor prices.
            let cfl = if cfl_allowed && !is_rect && block_has_chroma {
                self.select_cfl(r, c, bw, luma, tx_options[tx_depth].0)
            } else {
                None
            };
            (luma, tx_depth, cfl)
        };
        let luma_tx = tx_options[tx_depth].0;
        let LumaMode {
            mode: y_mode,
            angle_delta,
            filter_intra,
        } = luma;
        let (amode, lmode) = self.y_mode_ctx(r, c);
        self.sym.encode(
            usize::from(y_mode),
            self.cdfs.intra_frame_y_mode[amode][lmode].slot(),
        );
        // intra_angle_info_y (§5.11.42): for MiSize ≥ BLOCK_8X8 with a directional YMode, the fine
        // angle `AngleDeltaY ∈ [-3, 3]` is signaled (biased by MAX_ANGLE_DELTA = 3) under the
        // per-mode `Angle_Delta_Cdf`. 4×4 blocks never reach this (MiSize < BLOCK_8X8 ⇒ delta = 0).
        if bw >= 8 && is_directional(y_mode) {
            let sym = (i32::from(angle_delta) + 3) as usize;
            self.sym.encode(
                sym,
                self.cdfs.angle_delta[(y_mode - V_PRED) as usize].slot(),
            );
        }
        if !skip {
            let decided = Decided {
                at: (r, c, bw, bh),
                luma,
                tx_depth,
                cfl,
            };
            if self.sym.tally.is_some() {
                self.decided.push(decided);
            } else {
                self.sb_decisions.blocks.push(decided);
            }
        }
        let ym = usize::from(y_mode);
        // uv_mode. Its CDF is indexed by the luma mode. When CfL is allowed, the encoder picks
        // chroma-from-luma if it beats plain DC_PRED, then emits read_cfl_alphas; otherwise
        // uv_mode = DC_PRED.
        // §5.11.7 wraps `uv_mode` (and the `read_cfl_alphas` that follows UV_CFL_PRED) in
        // `if (HasChroma)`. A monochrome frame codes neither, and neither does a subsampled
        // sub-8x8 block whose chroma is coded by the covering block of its MI group.
        if !block_has_chroma {
            // no uv_mode symbol at all
        } else if cfl_allowed {
            let uv = if cfl.is_some() { UV_CFL_PRED } else { DC_PRED };
            self.sym
                .encode(usize::from(uv), self.cdfs.uv_mode_cfl_allowed[ym].slot());
            if let Some((au, av)) = cfl {
                self.emit_cfl_alphas(au, av);
            }
        } else {
            self.sym
                .encode(0, self.cdfs.uv_mode_cfl_not_allowed[ym].slot());
        }

        // palette_mode_info (§5.11.46): with allow_screen_content_tools an 8×8..64×64 block signals
        // `has_palette_y` (when YMode == DC_PRED) and `has_palette_uv` (when UVMode == DC_PRED). When
        // luma palette is selected, the palette size, the cache-usage flags, and the colors follow.
        // (Palette is only *chosen* for ≤32 — `palette` is `None` at 64 — but the flag is still coded.)
        if self.qindex > 0 && (8..=64).contains(&bw) {
            if y_mode == DC_PRED {
                let (bctx, pctx) = self.palette_y_ctx(r, c, bw, bh);
                self.sym.encode(
                    usize::from(palette.is_some()),
                    self.cdfs.palette_y_mode[bctx][pctx].slot(),
                );
                if let Some(pal) = &palette {
                    // palette_size_y_minus_2, then the colors (cache flags + new colors).
                    self.sym
                        .encode(pal.colors.len() - 2, self.cdfs.palette_y_size[bctx].slot());
                    let cache_n = self.palette_cache(r, c).len();
                    let colors = pal.colors.clone();
                    self.signal_palette_colors(&colors, cache_n);
                }
            }
            // UVMode == DC_PRED; the context is `(PaletteSizeY > 0)`. No chroma palette is used.
            // §5.11.46 also requires `HasChroma`, so a monochrome frame codes no `has_palette_uv`.
            if self.has_chroma() && cfl.is_none() {
                let uctx = usize::from(palette.is_some());
                self.sym.encode(0, self.cdfs.palette_uv_mode[uctx].slot());
            }
        }

        // filter_intra_mode_info (§5.11.24): a DC_PRED luma block ≤ 32px with no palette
        // (`PaletteSizeY == 0`) signals use_filter_intra, under the block-size CDF row. A 64×64 block
        // (`Max(w, h) > 32`) does not signal it.
        if self.qindex > 0 && y_mode == DC_PRED && palette.is_none() && bw <= 32 && bh <= 32 {
            let fi_cdf = match bw {
                4 => &mut self.cdfs.filter_intra_4x4,
                8 => &mut self.cdfs.filter_intra_8x8,
                16 => &mut self.cdfs.filter_intra_16x16,
                _ => &mut self.cdfs.filter_intra_32x32,
            };
            self.sym
                .encode(usize::from(filter_intra.is_some()), fi_cdf.slot());
            if let Some(fi) = filter_intra {
                self.sym
                    .encode(usize::from(fi), self.cdfs.filter_intra_mode.slot());
            }
        }

        // palette_tokens (§5.11.49): the color_index_map_y, coded after mode_info and before
        // read_block_tx_size.
        if let Some(pal) = &palette {
            let idx_map = pal.index_map.clone();
            self.signal_palette_tokens(&idx_map, bw, pal.colors.len());
        }

        // read_block_tx_size (§5.11.16): under TX_MODE_SELECT a lossy ≥8×8 luma block signals
        // `tx_depth`, choosing a luma transform `bw >> tx_depth` (square, ≥ 4×4). Chroma never signals
        // — for 4:4:4 it always uses the block-size transform (`Max_Tx_Size_Rect`). The CDF is keyed by
        // block size (`Max_Tx_Depth`); the context is `(aboveTxW ≥ bw) + (leftTxH ≥ bw)`.
        // A rectangular block's one transform (tx_depth = 0) is coded under the CDF of its
        // bounding square (`txSzSqrUp = Max(bw, bh)`); the context is `(aboveTxW ≥ bw) +
        // (leftTxH ≥ bh)`, exactly as for a square block of that size.
        if lossy_large {
            self.sym
                .encode(tx_depth, tx_depth_slot(&mut self.cdfs, bw.max(bh), tx_ctx));
        }

        // Per-MI bookkeeping: block size, luma mode (for neighbour contexts), and the luma
        // transform-size log2 — consumed by the deblocking loop filter (which derives the chroma tx
        // size from `mi_bsl` since 4:4:4 chroma uses the block-size transform).
        let txl = luma_tx.log2_width() as u8;
        let txl_h = luma_tx.log2_height() as u8;
        let (psize, pcolors) = match &palette {
            Some(pal) => {
                let mut buf = [0u16; 8];
                buf[..pal.colors.len()].copy_from_slice(&pal.colors);
                (pal.colors.len() as u8, buf)
            }
            None => (0u8, [0u16; 8]),
        };
        for y in 0..n4h {
            for x in 0..n4 {
                let (rr, cc) = (r + y, c + x);
                if rr < self.mi_rows && cc < self.mi_cols {
                    self.mi_bsl[rr * self.mi_cols + cc] = bsl;
                    self.mi_bsl_h[rr * self.mi_cols + cc] = bsl_h;
                    self.mi_ymode[rr * self.mi_cols + cc] = y_mode;
                    self.tx_log2[rr * self.mi_cols + cc] = txl;
                    self.tx_log2_h[rr * self.mi_cols + cc] = txl_h;
                    self.mi_skip[rr * self.mi_cols + cc] = u8::from(skip);
                    self.mi_psize[rr * self.mi_cols + cc] = psize;
                    self.mi_pcolors[rr * self.mi_cols + cc] = pcolors;
                    self.mi_segid[rr * self.mi_cols + cc] = segid as u8;
                    self.mi_dlf[rr * self.mi_cols + cc] = self.current_dlf as i8;
                }
            }
        }

        // A palette block reconstructs directly (luma = palette[ColorMapY], chroma = DC) with no
        // residual, then marks its luma cells decoded so later directional predictions see them.
        if let Some(pal) = palette {
            self.recon_palette(r, c, bw, &pal);
            self.mark_block_decoded(r, c, bw / 4, bw / 4);
            return;
        }

        // residual(): per plane. Luma uses the block's intra mode and signaled transform size (a
        // raster of `luma_tx` sub-transforms, each predicted + reconstructed in turn); 4:4:4 chroma is
        // DC_PRED (plus the CfL term) over the block-size transform — but chroma never uses TX_64X64,
        // so a 64×64 block's chroma is a 2×2 raster of TX_32X32. Luma (plane 0) is fully reconstructed
        // before chroma, so the chroma CfL reads finalized luma recon.
        // `block_has_chroma` already carries the frame-level `NumPlanes > 1`, so a monochrome
        // frame codes plane 0 alone and a subsampled block that shares its neighbour's chroma
        // codes no chroma residual (§5.11.34).
        for plane in 0..1 + 2 * usize::from(block_has_chroma) {
            let pred = Pred {
                mode: if plane == 0 { y_mode } else { DC_PRED },
                angle_delta: if plane == 0 { angle_delta } else { 0 },
                filter_intra: if plane == 0 { filter_intra } else { None },
                cfl_alpha: match (plane, cfl) {
                    (1, Some((au, _))) => Some(au),
                    (2, Some((_, av))) => Some(av),
                    _ => None,
                },
            };
            let ptx = if plane == 0 {
                luma_tx
            } else {
                self.chroma_tx(bw, bh)
            };
            self.code_plane(plane, r, c, bw, bh, ptx, pred, skip, Coding::Emit);
        }
    }

    /// The `(above, left)` mode contexts of the `intra_frame_y_mode` CDF for the block at MI
    /// `(r, c)` (§8.3.2): `Intra_Mode_Context` of each neighbour's `YMode`, `DC_PRED` past the tile.
    fn y_mode_ctx(&self, r: usize, c: usize) -> (usize, usize) {
        let above = if r > self.tile_r0 {
            usize::from(self.mi_ymode[(r - 1) * self.mi_cols + c])
        } else {
            usize::from(DC_PRED)
        };
        let left = if c > self.tile_c0 {
            usize::from(self.mi_ymode[r * self.mi_cols + (c - 1)])
        } else {
            usize::from(DC_PRED)
        };
        (
            cdf::INTRA_MODE_CONTEXT[above],
            cdf::INTRA_MODE_CONTEXT[left],
        )
    }

    /// The luma prediction candidates the search tries for a lossy `bw × bh` block, cheapest
    /// signalling first (so it wins ties).
    ///
    /// - A rectangular block: the `SMOOTH` family and `PAETH` only — staying off `DC_PRED` means
    ///   no `palette`/`use_filter_intra` flags follow, and off the directional modes keeps its
    ///   prediction on the rectangular non-directional path.
    /// - A 4×4 block: all 13 modes (`AngleDeltaY` is not coded below 8×8), then filter-intra.
    /// - A square ≥8×8 block: the non-directional modes, the search's directional modes at each of
    ///   its angle deltas, then filter-intra.
    fn luma_candidates(&self, bw: usize, bh: usize) -> Vec<LumaMode> {
        let plain = |mode: u8| LumaMode {
            mode,
            angle_delta: 0,
            filter_intra: None,
        };
        if bw != bh {
            return [SMOOTH_PRED, SMOOTH_V_PRED, SMOOTH_H_PRED, PAETH_PRED]
                .map(plain)
                .to_vec();
        }
        let mut out: Vec<LumaMode> = if bw == 4 {
            LUMA_MODES.iter().map(|&m| plain(m)).collect()
        } else {
            let mut v: Vec<LumaMode> = [
                DC_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
                PAETH_PRED,
            ]
            .map(plain)
            .to_vec();
            for &mode in self.search.directional {
                for &angle_delta in self.search.angle_deltas {
                    v.push(LumaMode {
                        mode,
                        angle_delta,
                        filter_intra: None,
                    });
                }
            }
            v
        };
        // §5.11.24 codes `use_filter_intra` only when `Max(Block_Width, Block_Height) <= 32`: a
        // 64x64 block cannot signal filter-intra, so it must not be offered one.
        let filter_intra_modes = if bw <= 32 {
            self.search.filter_intra_modes
        } else {
            0
        };
        out.extend((0..filter_intra_modes).map(|fi| LumaMode {
            filter_intra: Some(fi),
            ..DC_LUMA
        }));
        out
    }

    /// The `has_palette_y` contexts (§8.3.2) of the `bw × bh` block at MI `(r, c)`: `bsizeCtx =
    /// Mi_Width_Log2 + Mi_Height_Log2 - 2`, which also keys `palette_size_y_minus_2`, and the
    /// count of above and left neighbours (within the tile) that carry a luma palette. One
    /// derivation for both the signalled flag and [`Self::luma_mode_rate`]'s price of it, so the
    /// rate can only ever be read against the context the stream codes under.
    fn palette_y_ctx(&self, r: usize, c: usize, bw: usize, bh: usize) -> (usize, usize) {
        let bctx = bw.trailing_zeros() as usize + bh.trailing_zeros() as usize - 6;
        let above = r > self.tile_r0 && self.mi_psize[(r - 1) * self.mi_cols + c] > 0;
        let left = c > self.tile_c0 && self.mi_psize[r * self.mi_cols + (c - 1)] > 0;
        (bctx, usize::from(above) + usize::from(left))
    }

    /// The rate, in [`RATE_ONE`](super::coder::RATE_ONE) units, of signalling `cand` as the luma
    /// mode of the lossy `bw × bh` block at MI `(r, c)` (no palette): `y_mode`, then `angle_delta_y` (directional,
    /// ≥8×8), then for a `DC_PRED` block the `has_palette_y = 0` flag (8×8..64×64) and
    /// `use_filter_intra` with its mode (≤32×32), each against its CDF as it stands.
    fn luma_mode_rate(&self, r: usize, c: usize, bw: usize, bh: usize, cand: LumaMode) -> u32 {
        let (amode, lmode) = self.y_mode_ctx(r, c);
        let mut rate = symbol_rate(
            self.cdfs.intra_frame_y_mode[amode][lmode].probs(),
            usize::from(cand.mode),
        );
        if bw >= 8 && is_directional(cand.mode) {
            rate += symbol_rate(
                self.cdfs.angle_delta[usize::from(cand.mode - V_PRED)].probs(),
                (i32::from(cand.angle_delta) + 3) as usize,
            );
        }
        if cand.mode != DC_PRED {
            return rate;
        }
        if (8..=64).contains(&bw) {
            let (bctx, pctx) = self.palette_y_ctx(r, c, bw, bh);
            rate += symbol_rate(self.cdfs.palette_y_mode[bctx][pctx].probs(), 0);
        }
        if bw <= 32 && bh <= 32 {
            let flag = match bw {
                4 => &self.cdfs.filter_intra_4x4,
                8 => &self.cdfs.filter_intra_8x8,
                16 => &self.cdfs.filter_intra_16x16,
                _ => &self.cdfs.filter_intra_32x32,
            };
            rate += symbol_rate(flag.probs(), usize::from(cand.filter_intra.is_some()));
            if let Some(fi) = cand.filter_intra {
                rate += symbol_rate(self.cdfs.filter_intra_mode.probs(), usize::from(fi));
            }
        }
        rate
    }

    /// Picks the luma prediction for the lossy `bw × bh` block at MI `(r, c)`, and which of
    /// `txs` — the transforms its luma may be coded with, each with the rate of signalling it —
    /// codes it, by rate–distortion cost. Returns the mode and the index into `txs`, which for a
    /// square ≥8×8 block is its `tx_depth`.
    ///
    /// Every pairing of a [`Self::luma_candidates`] mode with a transform is trialled: its luma
    /// residual is coded transform by transform (each transform predicted from the reconstruction
    /// its predecessors left), and the reconstruction's squared error weighed against the
    /// coefficient rate plus the mode's and the transform's signalling rate by [`Self::rd_cost`].
    ///
    /// Every pairing is first trialled with `DCT_DCT` alone ([`Coding::Screen`]), and only the
    /// [`MODE_REFINE`] cheapest are trialled again with the transform-type search each would be
    /// coded with: that search multiplies a trial's transforms by up to five, and a pairing's
    /// `DCT_DCT` cost already tells the contenders from the rest — so choosing the transform depth
    /// as well multiplies only the screening pass, not the refining one. Both passes run even
    /// where they must agree — a list of at most [`MODE_REFINE`] pairings, or transforms with no
    /// type to choose — because a guard skipping the second pass there could only save time,
    /// never change the answer, and so could not be tested; that costs the `Fastest` search and
    /// 32×32/64×64 transforms up to eight repeated trials each.
    ///
    /// A tie goes to the cheaper `DCT_DCT` cost, then to the earlier pairing: the cheaper mode
    /// signalling, then the shallower transform split. The choice is a quality decision; every
    /// option reconstructs bit-exactly, so only the signaling must be correct.
    fn select_luma_mode(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        txs: &[(TxSize, u32)],
    ) -> (LumaMode, usize) {
        let candidates = self.luma_candidates(bw, bh);
        let pairs: Vec<(LumaMode, usize)> = candidates
            .iter()
            .flat_map(|&cand| (0..txs.len()).map(move |t| (cand, t)))
            .collect();
        let cost = |e: &mut Self, (cand, t): (LumaMode, usize), coding| {
            let (tx, tx_rate) = txs[t];
            e.luma_cost(r, c, bw, bh, tx, cand, coding) + e.rd_cost(0, tx_rate)
        };
        let mut costs: Vec<(u64, usize)> = (0..pairs.len())
            .map(|i| (cost(self, pairs[i], Coding::Screen), i))
            .collect();
        costs.sort_unstable();
        costs.truncate(MODE_REFINE);
        // `min_by_key` keeps the first of equal costs, and the list is in screening order: a tie
        // in full cost goes to the cheaper `DCT_DCT` cost, then to the earlier pairing.
        costs
            .into_iter()
            .map(|(_, i)| (cost(self, pairs[i], Coding::Trial), i))
            .min_by_key(|&(full, _)| full)
            .map_or((DC_LUMA, 0), |(_, i)| pairs[i])
    }

    /// The rate–distortion cost of coding the lossy `bw × bh` block at MI `(r, c)`'s luma as
    /// `cand` over `luma_tx` transforms: the trialled (under `coding`) residual's squared error
    /// and coefficient rate, plus the mode's signalling rate.
    #[allow(clippy::too_many_arguments)]
    fn luma_cost(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        luma_tx: TxSize,
        cand: LumaMode,
        coding: Coding,
    ) -> u64 {
        let pred = Pred {
            mode: cand.mode,
            angle_delta: cand.angle_delta,
            filter_intra: cand.filter_intra,
            cfl_alpha: None,
        };
        let (sse, rate) = self.trial_plane(0, r, c, bw, bh, luma_tx, pred, coding);
        self.rd_cost(sse, rate + self.luma_mode_rate(r, c, bw, bh, cand))
    }

    /// Picks chroma-from-luma alphas for the lossy square `bw × bw` block at MI `(r, c)`, whose
    /// luma is coded as `luma` over `luma_tx` transforms, by rate–distortion cost — or returns
    /// `None` when plain `DC_PRED` chroma is at least as cheap. Returns `(CflAlphaU, CflAlphaV)`,
    /// each in `-16..=16` and not both zero.
    ///
    /// CfL predicts from the block's own luma reconstruction, so that is trialled into place
    /// first. Then every alpha the search holds (`0`, then each magnitude at both signs) is
    /// trialled on each chroma plane, and the pair chosen jointly, since `cfl_alpha_signs` and the
    /// magnitude contexts couple the planes: each pair's chroma distortion against its coefficient
    /// rate plus `uv_mode = UV_CFL_PRED` and `read_cfl_alphas`, the all-zero pair against
    /// `uv_mode = DC_PRED` (and the `has_palette_uv = 0` that follows it). No alpha is ranked out
    /// by a cheaper proxy first, so a longer ladder can only find a cheaper pair. The trials are
    /// undone before returning. Earlier candidates win ties.
    fn select_cfl(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        luma: LumaMode,
        luma_tx: TxSize,
    ) -> Option<(i32, i32)> {
        // A search without CfL codes plain `DC_PRED` chroma and skips the trials below.
        let magnitudes = self.search.cfl_magnitudes?;
        // A block with no valid chroma residual simply gets no CfL; the residual loop is where
        // that (4:2:2-only) conformance violation is reported.
        plane_residual_size(bw, bw, 1, self.subsampling)?;
        let ctx = self.chroma_tx(bw, bw);
        let luma_state = self.save_plane(0, r, c, bw, bw);
        let luma_pred = Pred {
            mode: luma.mode,
            angle_delta: luma.angle_delta,
            filter_intra: luma.filter_intra,
            cfl_alpha: None,
        };
        self.code_plane(0, r, c, bw, bw, luma_tx, luma_pred, false, Coding::Trial);
        // `alpha = 0` (plain DC) first, then each magnitude at both signs.
        let alphas: Vec<i32> = std::iter::once(0)
            .chain(magnitudes.iter().flat_map(|&m| [m, -m]))
            .collect();
        // Per plane, `(alpha, SSE, rate)` for every alpha.
        let mut trials: [Vec<(i32, u64, u32)>; 2] = [Vec::new(), Vec::new()];
        for (plane, out) in (1..3).zip(trials.iter_mut()) {
            for &alpha in &alphas {
                let pred = Pred {
                    mode: DC_PRED,
                    angle_delta: 0,
                    filter_intra: None,
                    cfl_alpha: (alpha != 0).then_some(alpha),
                };
                let (sse, rate) = self.trial_plane(plane, r, c, bw, bw, ctx, pred, Coding::Trial);
                out.push((alpha, sse, rate));
            }
        }
        self.restore_plane(luma_state);

        let dc_rate = self.dc_chroma_rate(luma.mode, bw);
        let uv = self.cdfs.uv_mode_cfl_allowed[usize::from(luma.mode)].probs();
        let cfl_rate = symbol_rate(uv, usize::from(UV_CFL_PRED));
        let [u, v] = &trials;
        let (u0, v0) = (u[0], v[0]);
        let plain_dc = (self.rd_cost(u0.1 + v0.1, u0.2 + v0.2 + dc_rate), None);
        // Every pair but `(0, 0)` — index 0 of each list is `alpha = 0` — is a CfL candidate:
        // those with a nonzero U alpha, then those with U at 0 and a nonzero V alpha.
        let pairs = (1..u.len())
            .flat_map(|i| (0..v.len()).map(move |j| (i, j)))
            .chain((1..v.len()).map(|j| (0, j)));
        pairs
            .fold(plain_dc, |best, (i, j)| {
                let ((au, su, ru), (av, sv, rv)) = (u[i], v[j]);
                let rate = ru + rv + cfl_rate + self.cfl_alphas_rate(au, av);
                // `min_by_key` keeps its first argument on equal costs: earlier candidates, and
                // plain `DC_PRED` before them all, win ties.
                std::cmp::min_by_key(best, (self.rd_cost(su + sv, rate), Some((au, av))), |c| c.0)
            })
            .1
    }

    /// The mode-info rate [`Self::select_cfl`] charges plain `DC_PRED` chroma in a lossy square
    /// `bw`-wide block under luma mode `y_mode`, in [`RATE_ONE`](super::coder::RATE_ONE) units:
    /// `uv_mode = DC_PRED` under the CfL-allowed CDF, plus, for an 8×8..64×64 block (§5.11.46), the `has_palette_uv = 0`
    /// that follows it, priced at context 0. A function of its own so that the palette flag's
    /// share is assertable: inside `select_cfl` it only moves a close RD decision, and no fixture
    /// told adding it from subtracting it (#920).
    fn dc_chroma_rate(&self, y_mode: u8, bw: usize) -> u32 {
        let uv = self.cdfs.uv_mode_cfl_allowed[usize::from(y_mode)].probs();
        let mut rate = symbol_rate(uv, usize::from(DC_PRED));
        if (8..=64).contains(&bw) {
            rate += symbol_rate(self.cdfs.palette_uv_mode[0].probs(), 0);
        }
        rate
    }

    /// The rate of `read_cfl_alphas` (§5.11.45) for `(au, av)` against the CDFs as they stand,
    /// in [`RATE_ONE`](super::coder::RATE_ONE) units: what [`Self::emit_cfl_alphas`] codes.
    fn cfl_alphas_rate(&self, au: i32, av: i32) -> u32 {
        let (su, sv) = (cfl_sign(au), cfl_sign(av));
        let mut rate = symbol_rate(self.cdfs.cfl_sign.probs(), 3 * su + sv - 1);
        if su != 0 {
            let ctx = (su - 1) * 3 + sv;
            rate += symbol_rate(self.cdfs.cfl_alpha[ctx].probs(), (au.abs() - 1) as usize);
        }
        if sv != 0 {
            let ctx = (sv - 1) * 3 + su;
            rate += symbol_rate(self.cdfs.cfl_alpha[ctx].probs(), (av.abs() - 1) as usize);
        }
        rate
    }

    /// Emits `read_cfl_alphas` (§5.11.45): the joint `cfl_alpha_signs` symbol, then the per-plane
    /// `cfl_alpha_u`/`cfl_alpha_v` magnitudes (`|alpha| - 1`) for the non-zero planes, each under the
    /// sign-derived context.
    fn emit_cfl_alphas(&mut self, au: i32, av: i32) {
        let (su, sv) = (cfl_sign(au), cfl_sign(av));
        // signs + 1 = 3 * signU + signV  ⇒  cfl_alpha_signs = 3 * signU + signV - 1.
        let signs = 3 * su + sv - 1;
        self.sym.encode(signs, self.cdfs.cfl_sign.slot());
        if su != 0 {
            let ctx = (su - 1) * 3 + sv;
            self.sym
                .encode((au.abs() - 1) as usize, self.cdfs.cfl_alpha[ctx].slot());
        }
        if sv != 0 {
            let ctx = (sv - 1) * 3 + su;
            self.sym
                .encode((av.abs() - 1) as usize, self.cdfs.cfl_alpha[ctx].slot());
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::Planar8;

    use super::*;
    use crate::encoder::Effort;
    use crate::tile::coder::RATE_ONE;
    use crate::tile::tests::{
        Snapshot, at_effort, busy64, dims, grey4x4, luma16, planes_420_with, planes_with, primed,
    };
    /// `select_cfl`'s answer for the 8x8 block at the origin of `planes`, its luma coded `DC_PRED`
    /// over one 8x8 transform.
    fn cfl_at_origin(planes: &Planar8, effort: Effort) -> Option<(i32, i32)> {
        at_effort(planes, 40, effort).select_cfl(0, 0, 8, DC_LUMA, TxSize::Tx8x8)
    }

    #[test]
    fn the_cfl_alpha_search_follows_the_sign_of_the_correlation() {
        // `select_cfl` is a pure encoder decision — any alpha reconstructs bit-exactly, so no
        // decoder disagrees with it and only a direct test separates a working search from one
        // that has the sign wrong or that prices a pair it does not code.
        //
        // Chroma that tracks luma upward must pick a positive alpha; chroma that moves against it,
        // a negative one. Both planes carry the same pattern, so the two returned alphas agree.
        // Chroma centred on 128 — the DC prediction for a block with no neighbours — so the
        // baseline `alpha = 0` predicts the mean exactly and only the *shape* can beat it.
        //
        // Luma rises 8 per sample and is constant in y, so each 2x2 box sums to
        // `2 * (l + (l + 8))` and `L = sum << 1`: `L` rises **128** per chroma step. The
        // high-frequency term is `Round2Signed(alpha * dL, 6) = alpha * 2`, and chroma rises 4 per
        // step, so alpha 2 predicts the chroma exactly — from the luma *reconstruction*, which on
        // a ramp this smooth reproduces the source slope.
        let up = planes_420_with(16, 16, |p, x, _| {
            if p == 0 {
                (100 + x * 8) as u8
            } else {
                (122 + x * 4) as u8
            }
        });
        let (au, av) = cfl_at_origin(&up, Effort::Thorough).expect("correlated chroma wants CfL");
        assert_eq!(au, 2, "positively correlated chroma ⇒ alpha 2");
        assert_eq!(au, av, "both planes carry the same pattern");

        let down = planes_420_with(16, 16, |p, x, _| {
            if p == 0 {
                (100 + x * 8) as u8
            } else {
                (134 - x * 4) as u8
            }
        });
        let (au, _) =
            cfl_at_origin(&down, Effort::Thorough).expect("anti-correlated chroma wants CfL");
        assert_eq!(au, -2, "anti-correlated chroma ⇒ alpha -2");

        // Flat chroma has no high-frequency content to predict, so plain DC wins and no alpha is
        // signalled at all.
        let flat = planes_420_with(
            16,
            16,
            |p, x, _| {
                if p == 0 { (100 + x * 8) as u8 } else { 128 }
            },
        );
        assert_eq!(cfl_at_origin(&flat, Effort::Thorough), None);
    }

    #[test]
    fn the_cfl_search_chooses_each_planes_alpha_on_its_own() {
        // The pair is chosen jointly — the signs symbol couples the planes — but each plane's
        // distortion is its own: chroma U tracking luma and chroma V flat must give `(2, 0)`, and
        // the reverse `(0, 2)`, never the same alpha on both.
        let u_only = planes_420_with(16, 16, |p, x, _| match p {
            0 => (100 + x * 8) as u8,
            1 => (122 + x * 4) as u8,
            _ => 128,
        });
        assert_eq!(cfl_at_origin(&u_only, Effort::Thorough), Some((2, 0)));
        let v_only = planes_420_with(16, 16, |p, x, _| match p {
            0 => (100 + x * 8) as u8,
            1 => 128,
            _ => (122 + x * 4) as u8,
        });
        assert_eq!(cfl_at_origin(&v_only, Effort::Thorough), Some((0, 2)));
    }

    #[test]
    fn the_cfl_search_leaves_the_encoder_as_it_found_it() {
        // `select_cfl` trials the block's luma into place and every alpha on both chroma planes;
        // none of that may survive it, or the block the encoder then codes would predict from
        // samples no decoder has.
        let up = planes_420_with(16, 16, |p, x, _| match p {
            0 => (100 + x * 8) as u8,
            _ => (122 + x * 4) as u8,
        });
        let mut e = at_effort(&up, 40, Effort::Thorough);
        let before = Snapshot::of(&e);
        assert!(e.select_cfl(0, 0, 8, DC_LUMA, TxSize::Tx8x8).is_some());
        assert!(Snapshot::of(&e) == before, "select_cfl changed the encoder");
    }

    #[test]
    fn select_cfl_tries_only_the_preset_alpha_ladder() {
        // Chroma rising 4 per step against luma rising 128 in `L` wants alpha 2 (derived in the
        // test above). 2 is on the thinned `Fast` ladder, so that preset still finds it; the
        // `Fastest` preset searches no alpha at all and codes plain DC_PRED chroma.
        let up = planes_420_with(16, 16, |p, x, _| {
            if p == 0 {
                (100 + x * 8) as u8
            } else {
                (122 + x * 4) as u8
            }
        });
        assert_eq!(cfl_at_origin(&up, Effort::Fast), Some((2, 2)));
        assert_eq!(cfl_at_origin(&up, Effort::Fastest), None);

        // Chroma rising 6 per step wants alpha 3, which the thinned ladder does not hold: `Fast`
        // must settle on a ladder rung instead.
        let steep = planes_420_with(16, 16, |p, x, _| {
            if p == 0 {
                (100 + x * 8) as u8
            } else {
                (119 + x * 6) as u8
            }
        });
        assert_eq!(cfl_at_origin(&steep, Effort::Thorough), Some((3, 3)));
        let (au, _) =
            cfl_at_origin(&steep, Effort::Fast).expect("correlated chroma still wants CfL");
        assert!([2, 4].contains(&au), "alpha {au} is not on the Fast ladder");
    }

    /// Off-axis stripes: content directional prediction wins on, at a base angle plus a delta.
    fn stripes(x: f64, y: f64) -> f64 {
        128.0 + 90.0 * ((x * 0.8 + y * 0.55) / 2.3).sin() + 20.0 * (y / 5.0).cos()
    }

    /// Luma decisions of the square-block mode searches over a 64x64 frame (one superblock) whose
    /// luma is `luma(x, y)`, with the reconstruction primed to the source so every predictor sees
    /// real neighbours: each interior 8x8 (one 8x8 transform), then each interior 4x4.
    fn mode_decisions(effort: Effort, luma: fn(f64, f64) -> f64) -> (Vec<LumaMode>, Vec<LumaMode>) {
        const N: usize = 64;
        let mut rgb = vec![0u8; N * N * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let v = luma((i % N) as f64, (i / N) as f64);
            px.copy_from_slice(&[128, v as u8, 128]);
        }
        let p = Planar8::from_rgb8_identity(&rgb, dims(N as u32, N as u32)).unwrap();
        let mut e = at_effort(&p, 40, effort);
        let cw = e.geom[0].coded_w;
        for y in 0..N {
            for x in 0..N {
                e.recon[0][y * cw + x] = e.sample(0, x, y) as u16;
            }
        }
        e.block_decoded.iter_mut().for_each(|b| *b = 1);
        let mut nxn = Vec::new();
        let mut four = Vec::new();
        for r in (2..N / 4 - 2).step_by(2) {
            for c in (2..N / 4 - 2).step_by(2) {
                nxn.push(e.select_luma_mode(r, c, 8, 8, &[(TxSize::Tx8x8, 0)]).0);
            }
        }
        for r in 1..N / 4 - 1 {
            for c in 1..N / 4 - 1 {
                four.push(e.select_luma_mode(r, c, 4, 4, &[(TxSize::Tx4x4, 0)]).0);
            }
        }
        (nxn, four)
    }

    #[test]
    fn select_luma_mode_refines_angles_only_under_the_full_search() {
        let (thorough, _) = mode_decisions(Effort::Thorough, stripes);
        assert!(
            thorough.iter().any(|m| m.angle_delta != 0),
            "off-axis stripes take an angle refinement somewhere: {thorough:?}"
        );
        let (balanced, _) = mode_decisions(Effort::Balanced, stripes);
        assert!(balanced.iter().all(|m| m.angle_delta == 0), "{balanced:?}");
    }

    /// Smooth, curved shading: content the recursive filter-intra predictor wins on.
    fn shading(x: f64, y: f64) -> f64 {
        128.0 + 60.0 * (x / 9.0).sin() * (y / 11.0).cos() + 0.4 * x
    }

    #[test]
    fn filter_intra_is_tried_only_below_the_fast_preset() {
        let (nxn, four) = mode_decisions(Effort::Balanced, shading);
        assert!(
            nxn.iter().any(|d| d.filter_intra.is_some())
                && four.iter().any(|d| d.filter_intra.is_some()),
            "some block of each size prefers filter-intra: {nxn:?} {four:?}"
        );
        let (nxn, four) = mode_decisions(Effort::Fast, shading);
        assert!(nxn.iter().all(|d| d.filter_intra.is_none()), "{nxn:?}");
        assert!(four.iter().all(|d| d.filter_intra.is_none()), "{four:?}");
    }

    #[test]
    fn directional_modes_are_tried_only_below_the_fastest_preset() {
        // ≥8×8 only: `Fastest` codes no lossy 4×4 block (see `Search::directional`), so the 4×4
        // search has no pruned configuration to pin.
        let (nxn, _) = mode_decisions(Effort::Fast, stripes);
        assert!(
            nxn.iter().any(|d| is_directional(d.mode)),
            "stripes pick a directional mode: {nxn:?}"
        );
        let (nxn, _) = mode_decisions(Effort::Fastest, stripes);
        assert!(nxn.iter().all(|d| !is_directional(d.mode)), "{nxn:?}");
    }

    #[test]
    fn select_luma_mode_takes_the_cheapest_candidate_and_leaves_no_trace() {
        // The mode search trials every candidate's luma residual in place; the answer must be the
        // first candidate of least cost — its trialled distortion and coefficient rate plus its
        // signalling rate — and the encoder must come back exactly as it went in.
        const N: usize = 32;
        let mut rgb = vec![0u8; N * N * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let v = stripes((i % N) as f64, (i / N) as f64);
            px.copy_from_slice(&[128, v as u8, 128]);
        }
        let p = Planar8::from_rgb8_identity(&rgb, dims(N as u32, N as u32)).unwrap();
        let mut e = at_effort(&p, 60, Effort::Thorough);
        let cw = e.geom[0].coded_w;
        for y in 0..N {
            for x in 0..N {
                e.recon[0][y * cw + x] = e.sample(0, x, y) as u16;
            }
        }
        e.block_decoded.iter_mut().for_each(|b| *b = 1);
        // The 8x8 block's two transform depths carry different signalling rates, so the rate
        // each pairing adds is part of what is compared.
        let depths = [(TxSize::Tx8x8, 2 * RATE_ONE), (TxSize::Tx4x4, 5 * RATE_ONE)];
        for (r, c, bw, bh, txs) in [
            (2, 2, 8, 8, &depths[..]),
            (2, 2, 8, 8, &depths[1..]),
            (3, 5, 4, 4, &[(TxSize::Tx4x4, 0)][..]),
            (2, 2, 16, 8, &[(TxSize::Tx16x8, 0)][..]),
        ] {
            let before = Snapshot::of(&e);
            let chosen = e.select_luma_mode(r, c, bw, bh, txs);
            assert!(
                Snapshot::of(&e) == before,
                "select_luma_mode changed the encoder"
            );
            // The procedure, spelled out: every (mode, transform) pairing, mode-major, is priced
            // under `DCT_DCT` alone plus the transform's signalling rate; the `MODE_REFINE`
            // cheapest (earlier first on a tie), in that order, are re-priced in full; the first
            // of least cost wins.
            let pairs: Vec<(LumaMode, usize)> = e
                .luma_candidates(bw, bh)
                .into_iter()
                .flat_map(|m| (0..txs.len()).map(move |t| (m, t)))
                .collect();
            let mut price = |(m, t): (LumaMode, usize), coding| {
                let (tx, rate) = txs[t];
                e.luma_cost(r, c, bw, bh, tx, m, coding) + e.rd_cost(0, rate)
            };
            let screen: Vec<u64> = pairs.iter().map(|&p| price(p, Coding::Screen)).collect();
            let mut contenders: Vec<usize> = (0..pairs.len()).collect();
            contenders.sort_by_key(|&i| (screen[i], i));
            contenders.truncate(MODE_REFINE);
            let full: Vec<u64> = contenders
                .iter()
                .map(|&i| price(pairs[i], Coding::Trial))
                .collect();
            let least = *full.iter().min().unwrap();
            let first = contenders[full.iter().position(|&f| f == least).unwrap()];
            assert_eq!(chosen, pairs[first], "{r},{c} {bw}x{bh}");
        }
    }

    #[test]
    fn luma_candidates_follow_the_search_and_the_block_shape() {
        let p = grey4x4();
        let count = |effort, bw, bh| at_effort(&p, 40, effort).luma_candidates(bw, bh).len();
        // A rectangular block: the SMOOTH family and PAETH, whatever the search.
        assert_eq!(count(Effort::Thorough, 16, 8), 4);
        assert_eq!(count(Effort::Fastest, 8, 16), 4);
        // 4x4: all 13 modes, then five filter-intra modes under the full search.
        assert_eq!(count(Effort::Thorough, 4, 4), 13 + 5);
        assert_eq!(count(Effort::Fast, 4, 4), 13);
        // ≥8x8: five non-directional modes, 8 directional × 7 deltas, five filter-intra modes.
        assert_eq!(count(Effort::Thorough, 8, 8), 5 + 56 + 5);
        assert_eq!(count(Effort::Balanced, 16, 16), 5 + 8 + 5);
        assert_eq!(count(Effort::Fastest, 32, 32), 5);
        // Filter-intra up to 32x32 only (§5.11.24): a 64x64 block cannot signal it.
        assert_eq!(count(Effort::Thorough, 32, 32), 5 + 56 + 5);
        assert_eq!(count(Effort::Thorough, 64, 64), 5 + 56);
        // Cheapest signalling first: `DC_PRED` opens every square list, the rectangle's opens
        // with `SMOOTH_PRED`, and filter-intra closes them.
        let e = at_effort(&p, 40, Effort::Thorough);
        assert_eq!(e.luma_candidates(8, 8)[0], DC_LUMA);
        assert_eq!(e.luma_candidates(4, 4)[0], DC_LUMA);
        assert_eq!(e.luma_candidates(16, 8)[0].mode, SMOOTH_PRED);
        let last = *e.luma_candidates(8, 8).last().unwrap();
        assert_eq!((last.mode, last.filter_intra), (DC_PRED, Some(4)));
    }

    #[test]
    fn cfl_sign_is_the_spec_sign_code() {
        assert_eq!([cfl_sign(0), cfl_sign(-3), cfl_sign(5)], [0, 1, 2]);
    }

    #[test]
    fn dc_chroma_rate_adds_the_palette_uv_flag_from_8x8_to_64x64() {
        let p = grey4x4();
        let e = FrameEncoder::new(&p, 40);
        let uv = symbol_rate(
            e.cdfs.uv_mode_cfl_allowed[usize::from(D67_PRED)].probs(),
            usize::from(DC_PRED),
        );
        let flag = symbol_rate(e.cdfs.palette_uv_mode[0].probs(), 0);
        assert!(flag > 0, "a free flag would hide its sign");
        // A 4×4 codes no `has_palette_uv`; 8×8 and 64×64 bound the blocks that do.
        assert_eq!(e.dc_chroma_rate(D67_PRED, 4), uv);
        assert_eq!(e.dc_chroma_rate(D67_PRED, 8), uv + flag);
        assert_eq!(e.dc_chroma_rate(D67_PRED, 64), uv + flag);
    }

    #[test]
    fn luma_mode_rate_prices_each_mode_info_element_it_codes() {
        let p = grey4x4();
        let e = FrameEncoder::new(&p, 40);
        let y = |m: u8| symbol_rate(e.cdfs.intra_frame_y_mode[0][0].probs(), usize::from(m));
        let plain = |mode| LumaMode {
            mode,
            angle_delta: 0,
            filter_intra: None,
        };
        let angle = |m: u8, d: i8| {
            symbol_rate(
                e.cdfs.angle_delta[usize::from(m - V_PRED)].probs(),
                (d + 3) as usize,
            )
        };
        let no_palette = |bctx: usize| symbol_rate(e.cdfs.palette_y_mode[bctx][0].probs(), 0);
        let fi_flag = |set: bool| symbol_rate(e.cdfs.filter_intra_8x8.probs(), usize::from(set));
        // A 4x4 codes no angle delta and no palette flag; a rectangle no filter-intra flag.
        assert_eq!(e.luma_mode_rate(0, 0, 4, 4, plain(D67_PRED)), y(D67_PRED));
        assert_eq!(
            e.luma_mode_rate(0, 0, 16, 8, plain(PAETH_PRED)),
            y(PAETH_PRED)
        );
        // ≥8x8 directional: the angle delta follows.
        let tilted = LumaMode {
            angle_delta: -2,
            ..plain(D113_PRED)
        };
        assert_eq!(
            e.luma_mode_rate(0, 0, 8, 8, tilted),
            y(D113_PRED) + angle(D113_PRED, -2)
        );
        // `DC_PRED` at 8x8: `has_palette_y = 0`, then `use_filter_intra`, then its mode.
        assert_eq!(
            e.luma_mode_rate(0, 0, 8, 8, DC_LUMA),
            y(DC_PRED) + no_palette(0) + fi_flag(false)
        );
        let fi = LumaMode {
            filter_intra: Some(3),
            ..DC_LUMA
        };
        assert_eq!(
            e.luma_mode_rate(0, 0, 8, 8, fi),
            y(DC_PRED)
                + no_palette(0)
                + fi_flag(true)
                + symbol_rate(e.cdfs.filter_intra_mode.probs(), 3)
        );
        // Each filter-intra size row is its own CDF: 16x16 does not borrow the 32x32 row.
        assert_eq!(
            e.luma_mode_rate(0, 0, 16, 16, DC_LUMA),
            y(DC_PRED) + no_palette(2) + symbol_rate(e.cdfs.filter_intra_16x16.probs(), 0)
        );
        // `DC_PRED` at 64x64 has a palette flag but no filter-intra flag, and neither does a
        // 64x32: §5.11.24 asks `Max(w, h) <= 32`, so one long side is enough to drop it.
        assert_eq!(
            e.luma_mode_rate(0, 0, 64, 64, DC_LUMA),
            y(DC_PRED) + no_palette(6)
        );
        assert_eq!(
            e.luma_mode_rate(0, 0, 64, 32, DC_LUMA),
            y(DC_PRED) + no_palette(5)
        );
    }

    #[test]
    fn cfl_alphas_rate_is_what_emitting_the_alphas_tallies() {
        let p = grey4x4();
        let mut e = FrameEncoder::new(&p, 40);
        for (au, av) in [(1, 0), (0, -4), (3, -16), (-7, 9), (16, 16)] {
            let want = e.cfl_alphas_rate(au, av);
            e.sym.tally = Some(0);
            e.emit_cfl_alphas(au, av);
            assert_eq!(e.sym.tally.take(), Some(want), "({au}, {av})");
        }
    }

    #[test]
    fn a_decided_block_is_coded_at_its_decided_transform_depth() {
        // A block the search has trialled is coded with the mode and `tx_depth` the trial
        // reached, which the per-MI transform map then carries.
        let busy = busy64();
        for (tx_depth, txl) in [(0, 4), (1, 3), (2, 2)] {
            let mut e = primed(&busy, 40);
            e.decided.push(Decided {
                at: (0, 0, 16, 16),
                luma: LumaMode {
                    mode: PAETH_PRED,
                    ..DC_LUMA
                },
                tx_depth,
                cfl: None,
            });
            e.encode_block(0, 0, 16, 16);
            assert_eq!(e.tx_log2[0], txl, "tx_depth {tx_depth}");
            assert_eq!(e.mi_ymode[0], PAETH_PRED);
            assert!(e.decided.is_empty(), "consumed for real");
        }
    }

    #[test]
    fn a_skip_block_neither_records_nor_takes_a_decision() {
        // Mid-grey everywhere is flat and DC-predictable at the frame origin, so the block is
        // skipped: coded `DC_PRED` whatever a trial of it once decided, and recorded by no trial.
        let grey = luma16(|_, _| 128);
        let mut e = primed(&grey, 40);
        let planted = Decided {
            at: (0, 0, 16, 16),
            luma: LumaMode {
                mode: PAETH_PRED,
                ..DC_LUMA
            },
            tx_depth: 2,
            cfl: None,
        };
        e.decided.push(planted);
        e.encode_block(0, 0, 16, 16);
        assert_eq!(e.mi_skip[0], 1, "the fixture is a skip block");
        assert_eq!(
            (e.mi_ymode[0], e.tx_log2[0]),
            (DC_PRED, 4),
            "not as decided"
        );
        assert_eq!(e.decided.len(), 1, "left for no one");
        let mut e = primed(&grey, 40);
        e.sym.tally = Some(0);
        e.encode_block(0, 0, 16, 16);
        assert!(
            e.decided.is_empty(),
            "a trialled skip block records nothing"
        );
    }

    #[test]
    fn a_flat_block_is_skippable_and_a_varying_one_is_not() {
        // `block_is_skippable` decides whether a block can be coded with no residual at all. It is
        // an encoder choice, so no decoder disagrees with it — only a direct test can tell a
        // working implementation from one that always answers `false`.
        // 128 specifically: the block also has to match its own DC prediction, and the top-left
        // block has no neighbours, so §7.11.2.5 predicts `1 << (BitDepth - 1)`. A flat block of any
        // other value has a non-zero residual and is correctly *not* skippable.
        let flat = planes_420_with(16, 16, |_, _, _| 128);
        let e = FrameEncoder::new(&flat, 40);
        assert!(
            e.block_is_skippable(0, 0, 8),
            "a block matching its DC prediction is skippable"
        );
        let off_dc = planes_420_with(16, 16, |_, _, _| 77);
        let e = FrameEncoder::new(&off_dc, 40);
        assert!(
            !e.block_is_skippable(0, 0, 8),
            "flat but not equal to the DC prediction"
        );

        // A single differing luma sample inside the block is enough to disqualify it.
        let bumpy = planes_420_with(
            16,
            16,
            |p, x, y| {
                if p == 0 && (x, y) == (3, 3) { 200 } else { 128 }
            },
        );
        let e = FrameEncoder::new(&bumpy, 40);
        assert!(!e.block_is_skippable(0, 0, 8));

        // …and so is one in *chroma* only, which is what makes the per-plane extent load-bearing:
        // chroma (1,1) lies inside the block's 4x4 chroma residual, while chroma (5,5) is outside
        // it and must not affect the answer.
        let chroma_bump =
            planes_420_with(
                16,
                16,
                |p, x, y| {
                    if p == 1 && (x, y) == (1, 1) { 200 } else { 128 }
                },
            );
        let e = FrameEncoder::new(&chroma_bump, 40);
        assert!(!e.block_is_skippable(0, 0, 8));
        let outside = planes_420_with(
            16,
            16,
            |p, x, y| {
                if p == 1 && (x, y) == (5, 5) { 200 } else { 128 }
            },
        );
        let e = FrameEncoder::new(&outside, 40);
        assert!(e.block_is_skippable(0, 0, 8));
    }

    #[test]
    fn a_skippable_block_is_coded_with_dc_pred_whatever_the_mode_rates() {
        // `encode_block` codes a `block_is_skippable` block `skip = 1` with DC_PRED, without the
        // rate–distortion search: that search trials a residual a skip block never codes. With the
        // mode CDF adapted towards V_PRED — whose prediction of the frame's first block is the
        // §7.11.2 `(1 << (BitDepth - 1)) - 1` above row, one off the flat source — a search left to
        // run picks V_PRED and the skipped block reconstructs as 127 instead of 128. `qindex` 200
        // makes a bit of rate outweigh that one-step error (at 40 the search keeps DC_PRED anyway).
        let flat = planes_420_with(16, 16, |_, _, _| 128);
        let mut e = at_effort(&flat, 200, Effort::Thorough);
        e.clear_block_decoded(0, 0);
        assert!(e.block_is_skippable(0, 0, 8));
        let (amode, lmode) = e.y_mode_ctx(0, 0);
        for _ in 0..64 {
            e.sym.encode(
                usize::from(V_PRED),
                e.cdfs.intra_frame_y_mode[amode][lmode].slot(),
            );
        }
        e.encode_block(0, 0, 8, 8);
        assert_eq!(e.mi_ymode[0], DC_PRED);
        let cw = e.geom[0].coded_w;
        for y in 0..8 {
            assert!(
                e.recon[0][y * cw..y * cw + 8].iter().all(|&v| v == 128),
                "row {y} reconstructs the source"
            );
        }
    }

    #[test]
    fn skippability_is_decided_at_the_addressed_block() {
        // `block_is_skippable` picks `skip = 1`, which changes what is coded. Both the answer and
        // the `(c * 4, r * 4)` MI-to-sample conversion locating the block are invisible to the
        // oracle suite: every choice reconstructs bit-exactly, so a decision taken about the
        // *wrong* block still round-trips. Driving it directly is what makes the position
        // observable.
        //
        // Columns 0..16 are zero, 16.. are a gradient. MI column 4 is sample column 16, so MI (0, 4)
        // is non-flat while MI (0, 1) — sample column 4 — is flat. `c + 4` or `c / 4` would address
        // a different column and invert both answers.
        let p = planes_with(
            32,
            8,
            |x, _| if x < 16 { 0 } else { 70 + x as u8 },
            |_, _| 0,
        );
        let e = FrameEncoder::new(&p, 40);
        assert!(e.block_is_skippable(1, 1, 4), "flat block at sample x=4");
        assert!(
            !e.block_is_skippable(1, 4, 4),
            "gradient block at sample x=16"
        );

        // The same asymmetry vertically, so `r * 4` is pinned independently of `c * 4`.
        let p = planes_with(
            32,
            32,
            |_, y| if y < 16 { 0 } else { 70 + y as u8 },
            |_, _| 0,
        );
        let e = FrameEncoder::new(&p, 40);
        assert!(e.block_is_skippable(1, 1, 4), "flat block at sample y=4");
        assert!(
            !e.block_is_skippable(4, 1, 4),
            "gradient block at sample y=16"
        );

        // A qualifying block exists at all — the case that vanishes if the function is hardwired
        // to `false`. A non-flat chroma plane alone disqualifies it, which is the part of the loop
        // a monochrome frame no longer runs.
        let p = planes_with(16, 16, |_, _| 0, |_, _| 0);
        let e = FrameEncoder::new(&p, 40);
        assert!(e.block_is_skippable(2, 2, 4));
        let p = planes_with(16, 16, |_, _| 0, |x, y| u8::from(x == 9 && y == 9));
        let e = FrameEncoder::new(&p, 40);
        assert!(
            !e.block_is_skippable(2, 2, 4),
            "a single off chroma sample disqualifies the block"
        );
    }

    #[test]
    fn neg_interleave_inverts_neg_deinterleave() {
        // §5.11.9 decoder side; neg_interleave is its exact inverse, so the round-trip must recover
        // every segment id for every reference and segment count. Pins the whole mapping — any flipped
        // comparison or arithmetic breaks the round-trip for some (x, ref, max).
        fn neg_deinterleave(diff: i32, r: i32, max: i32) -> i32 {
            if r == 0 {
                return diff;
            }
            if r >= max - 1 {
                return max - diff - 1;
            }
            let bounded_limit = if 2 * r < max {
                2 * r
            } else {
                2 * (max - r - 1)
            };
            if diff <= bounded_limit {
                if diff & 1 == 0 {
                    r - (diff >> 1)
                } else {
                    r + ((diff + 1) >> 1)
                }
            } else if 2 * r < max {
                diff
            } else {
                max - (diff + 1)
            }
        }
        for max in [2, 3, 5, 8] {
            for r in 0..max {
                for x in 0..max {
                    let diff = neg_interleave(x, r, max);
                    assert!(
                        (0..max).contains(&diff),
                        "neg_interleave({x},{r},{max}) = {diff} out of [0,{max})"
                    );
                    assert_eq!(
                        neg_deinterleave(diff, r, max),
                        x,
                        "round-trip x={x} r={r} max={max} (diff={diff})"
                    );
                }
            }
        }
    }
}
