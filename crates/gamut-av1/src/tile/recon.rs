//! Residual coding and reconstruction: one plane's transform blocks predicted, transformed,
//! quantized, coded and reconstructed exactly as a decoder will (§7.12, §7.13), the transform
//! type and size choices, and the distortion and rate–distortion cost every trial is ranked by.

use std::sync::Arc;

use gamut_color::ChromaSubsampling;
use gamut_dsp::math::clip_pixel;

use super::coder::{Coding, RATE_ONE};
use super::intra::{FILTER_INTRA_MODE_TO_INTRA_DIR, Pred};
use super::{DC_PRED, FrameEncoder, SbDecisions};
use crate::filter::FrameFilters;
use crate::geom::{PlaneGeom, plane_residual_size};
use crate::quant::{ac_q, dequant, quantize};
use crate::transform::{TxSize, TxType, forward_transform_2d, inverse_transform_2d};

/// The `TX_SET_INTRA_2` types tried after `DCT_DCT`, with their `Tx_Type_Intra_Inv_Set2` symbols,
/// in the order the full search has always tried them (which decides ties): a pruned search keeps
/// a prefix, so `ADST_ADST` is the one extra type a two-type search tries.
pub(super) const TX_TYPE_TRIALS: [(TxType, usize); 4] = [
    (TxType::AdstAdst, 2),
    (TxType::AdstDct, 3),
    (TxType::DctAdst, 4),
    (TxType::Idtx, 0),
];

/// The square transform size of the given side length (4/8/16/32). Used to map a `bw >> tx_depth`
/// width back to a [`TxSize`] under `TX_MODE_SELECT`.
pub(super) const fn square_tx(width: usize) -> TxSize {
    match width {
        4 => TxSize::Tx4x4,
        8 => TxSize::Tx8x8,
        16 => TxSize::Tx16x16,
        32 => TxSize::Tx32x32,
        _ => TxSize::Tx64x64,
    }
}

/// The transform size that exactly covers a `w × h` block (the rectangular block's `Max_Tx_Size`
/// under `TX_MODE_LARGEST`): a square block maps to its square transform, a rectangular one to the
/// matching rectangular transform.
/// §5.11.37 `get_tx_size` for a chroma plane: the transform matching the plane's residual block,
/// capped at 32 because chroma never uses a 64-sample transform. A 64-sample axis folds to 32,
/// which is what makes a 64x64 block's 4:4:4 chroma a 2x2 raster of TX_32X32.
pub(crate) fn chroma_tx_size(rw: usize, rh: usize) -> TxSize {
    let uv = max_tx_size_rect(rw, rh);
    if uv.width() == 64 || uv.height() == 64 {
        if uv.width() == 16 {
            TxSize::Tx16x32
        } else if uv.height() == 16 {
            TxSize::Tx32x16
        } else {
            TxSize::Tx32x32
        }
    } else {
        uv
    }
}

/// `(Tx_Width_Log2, Tx_Height_Log2)` of the chroma transform covering a `bw` x `bh` luma block —
/// the deblock filter's view of §5.11.37, derived from the per-MI block-size maps it already has.
///
/// `None` mirrors [`plane_residual_size`]'s `BLOCK_INVALID`.
pub(crate) fn chroma_tx_log2(bw: usize, bh: usize, ss: ChromaSubsampling) -> Option<(u32, u32)> {
    let (rw, rh) = plane_residual_size(bw, bh, 1, ss)?;
    let tx = chroma_tx_size(rw, rh);
    Some((tx.log2_width(), tx.log2_height()))
}

pub(super) fn max_tx_size_rect(w: usize, h: usize) -> TxSize {
    // The 128-sample axes clamp to 64: there is no transform wider or taller than that, which is
    // why `Max_Tx_Size_Rect` maps BLOCK_64X128/128X64/128X128 all onto TX_64X64. Written as `min`
    // rather than a comparison: both arms agree at exactly 64, so any `<`/`<=` spelling of it is
    // the same function and could never be told apart by a test.
    let (w, h) = (w.min(64), h.min(64));
    match (w, h) {
        (4, 4) => TxSize::Tx4x4,
        (4, 8) => TxSize::Tx4x8,
        (4, 16) => TxSize::Tx4x16,
        (8, 4) => TxSize::Tx8x4,
        (8, 8) => TxSize::Tx8x8,
        (8, 16) => TxSize::Tx8x16,
        (8, 32) => TxSize::Tx8x32,
        (16, 4) => TxSize::Tx16x4,
        (16, 8) => TxSize::Tx16x8,
        (16, 16) => TxSize::Tx16x16,
        (16, 32) => TxSize::Tx16x32,
        (16, 64) => TxSize::Tx16x64,
        (32, 8) => TxSize::Tx32x8,
        (32, 16) => TxSize::Tx32x16,
        (32, 32) => TxSize::Tx32x32,
        (32, 64) => TxSize::Tx32x64,
        (64, 16) => TxSize::Tx64x16,
        (64, 32) => TxSize::Tx64x32,
        (64, 64) => TxSize::Tx64x64,
        // AV1 has no block whose aspect ratio exceeds 4:1, and every caller passes a block (or a
        // §5.11.38 residual block) the partition search built, so no other pair is constructible.
        // The invariant is asserted in debug builds; a library path does not panic, so a release
        // build falls back to the smallest transform rather than abort the caller's process.
        _ => {
            debug_assert!(false, "no transform matches a {w}x{h} block");
            TxSize::Tx4x4
        }
    }
}

/// The reconstructed (decoded) planes on the coded MI grid, used as prediction neighbours in the
/// lossy path and exported for the bit-exact decoder cross-check.
pub(crate) struct Reconstruction {
    /// After deblocking and CDEF (lossy path), before loop restoration.
    pub planes: [Vec<u16>; 3],
    /// Sample geometry of each plane; `planes[i]` is `geom[i].len()` samples strided by
    /// `geom[i].coded_w`.
    pub geom: [PlaneGeom; 3],
    /// Bits per sample (8, 10, or 12); the planes carry values in `0..=(1 << bit_depth) - 1`.
    pub bit_depth: u32,
    /// Post-deblock (pre-CDEF) luma, retained for the loop-restoration stripe boundaries. Loop
    /// restoration is applied by the caller **after** any superres upscale (§7.4 order:
    /// deblock → CDEF → superres → loop restoration), so both the CDEF luma (`planes[0]`) and this
    /// are upscaled first when superres is active. Empty on the lossless path.
    pub deblocked_luma: Vec<u16>,
    /// The deblocking level and CDEF strengths `planes` was filtered with, for the frame header
    /// to signal. All off on the lossless path, whose header signals no filter.
    pub filters: FrameFilters,
    /// Every superblock's coded decisions, raster order over the frame's superblock grid, for a
    /// second pass to replay.
    pub decisions: Arc<Vec<SbDecisions>>,
}

/// The divisor of `q²` that gives the rate–distortion multiplier `λ` (see
/// [`FrameEncoder::rd_cost`]).
const LAMBDA_DIV: u64 = 1024;

/// Where a transform block sits, and what its coefficient contexts and transform-type CDF need.
struct TxSite {
    plane: usize,
    sx: usize,
    sy: usize,
    /// The residual block the transform belongs to, for the `all_zero` context.
    block_w: usize,
    block_h: usize,
    tx_size: TxSize,
    /// `intraDir` (§9.4), which keys the 16×16 transform-type CDF.
    intra_dir: usize,
}

/// What coding one plane's residual for one block can change, captured by
/// [`FrameEncoder::save_plane`] so a trial can be undone.
pub(super) struct PlaneState {
    plane: usize,
    /// The block's in-plane rectangle and its reconstruction before the trial.
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    recon: Vec<u16>,
    /// The coefficient contexts over the rectangle's columns (`above_*`) and rows (`left_*`).
    above_level: Vec<u8>,
    above_dc: Vec<u8>,
    left_level: Vec<u8>,
    left_dc: Vec<u8>,
    block_decoded: Vec<u8>,
    max_luma: (usize, usize),
}

/// A transform block coded under the transform type [`FrameEncoder::select_tx_type`] chose.
struct TxChoice {
    /// The `Tx_Type_Intra_Inv_Set2` symbol of the chosen type (`IDTX` 0, `DCT_DCT` 1, `ADST_ADST`
    /// 2, `ADST_DCT` 3, `DCT_ADST` 4), which identifies it.
    symbol: usize,
    /// The quantized coefficient levels.
    levels: Vec<i32>,
    /// The reconstructed samples, row-major over the transform block.
    recon: Vec<u16>,
    /// Squared error of `recon` against the source.
    sse: u64,
    /// The coefficients' rate, in [`RATE_ONE`] units.
    rate: u32,
    /// The `culLevel`/`dcCategory` the block leaves in the neighbour contexts.
    cul: u8,
    dc: u8,
}

impl FrameEncoder<'_> {
    /// The transform a `bw × bh` block's chroma residual is coded with: the block-size transform
    /// on the lossy path (capped at 32, §5.11.37), 4×4 below 8×8 and on the lossless path.
    pub(super) fn chroma_tx(&self, bw: usize, bh: usize) -> TxSize {
        match plane_residual_size(bw, bh, 1, self.subsampling) {
            Some((rw, rh)) if self.qindex > 0 && bw.min(bh) >= 8 => chroma_tx_size(rw, rh),
            _ => TxSize::Tx4x4,
        }
    }

    /// Codes (or, under [`Coding::Trial`], only reconstructs and prices) `plane`'s residual for
    /// the `bw × bh` block at MI `(r, c)`: the raster of `ptx` transform blocks covering the plane's
    /// residual block, each predicted from the reconstruction its predecessors left. Returns the
    /// summed `(SSE, rate)` of [`Self::transform_block`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn code_plane(
        &mut self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        ptx: TxSize,
        pred: Pred,
        skip: bool,
        coding: Coding,
    ) -> (u64, u32) {
        let g = self.geom[plane];
        // §5.11.38: the residual block for this plane, which is a table lookup and not
        // `bw >> ss_x` — it clamps to a 4-sample minimum.
        let Some((rw, rh)) = plane_residual_size(bw, bh, plane, self.subsampling) else {
            // `BLOCK_INVALID`. Only reachable under 4:2:2 for a taller-than-wide block, which
            // §6.10.4 forbids and the partition search never emits (it drops `PARTITION_VERT`
            // there). Asserted in debug builds; a library path does not panic, so a release
            // build codes no residual for the plane rather than abort the caller's process.
            debug_assert!(
                false,
                "no plane-{plane} residual size for a {bw}x{bh} block"
            );
            return (0, 0);
        };
        let (pw, ph) = (ptx.width(), ptx.height());
        // §5.11.34 `baseXBlock`/`baseYBlock`: the MI position is scaled into the plane *before*
        // it is converted to samples, so an odd MI column lands on the covering chroma block.
        let base_x = (c >> g.ss_x) * 4;
        let base_y = (r >> g.ss_y) * 4;
        let (mut sse, mut rate) = (0, 0);
        // The transform grid is walked as ranges stepped by the transform size, not as cursors
        // advanced by it: a cursor loop (`while sy < … { …; sy += ph }`) never ends if the step
        // is 0, which a mutated `TxSize::height`/`width` produced as a suite TIMEOUT (#910);
        // `step_by(0)` panics instead. The rewrite also removes the cursor's `<`/`+=` mutants.
        for sy in (base_y..base_y + rh).step_by(ph) {
            for sx in (base_x..base_x + rw).step_by(pw) {
                if sx < g.coded_w && sy < g.coded_h {
                    let (d, r) =
                        self.transform_block(plane, sx, sy, rw, rh, ptx, pred, skip, coding);
                    sse += d;
                    rate += r;
                }
                // BlockDecoded (§5.11.34) is updated after each *transform* block (not the whole
                // block), so a later luma sub-transform's directional prediction sees the
                // above-right / below-left siblings just reconstructed. Luma-grid only (chroma is
                // never directional); marked on the luma pass.
                if plane == 0 {
                    self.mark_block_decoded(sy / 4, sx / 4, ph / 4, pw / 4);
                }
            }
        }
        (sse, rate)
    }

    /// Trials `plane`'s residual for the block at MI `(r, c)` under `pred`, then puts back
    /// everything the trial touched — the plane's reconstruction over the block, its coefficient
    /// contexts, `BlockDecoded` and the CfL luma bounds — so the encoder is left exactly as it was.
    /// Returns the `(SSE, rate)` the trial measured.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn trial_plane(
        &mut self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        ptx: TxSize,
        pred: Pred,
        coding: Coding,
    ) -> (u64, u32) {
        let saved = self.save_plane(plane, r, c, bw, bh);
        let measured = self.code_plane(plane, r, c, bw, bh, ptx, pred, false, coding);
        self.restore_plane(saved);
        measured
    }

    /// Captures what coding `plane`'s residual for the block at MI `(r, c)` can change.
    pub(super) fn save_plane(
        &self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
    ) -> PlaneState {
        let g = self.geom[plane];
        let (rw, rh) = plane_residual_size(bw, bh, plane, self.subsampling).unwrap_or((0, 0));
        let x0 = (c >> g.ss_x) * 4;
        let y0 = (r >> g.ss_y) * 4;
        // The rows and columns of the block that lie on the coded plane: a block overhanging the
        // frame codes only its in-plane transforms, and those never reach past the coded edge.
        let w = rw.min(g.coded_w.saturating_sub(x0));
        let h = rh.min(g.coded_h.saturating_sub(y0));
        let mut recon = Vec::with_capacity(w * h);
        for y in y0..y0 + h {
            let at = y * g.coded_w + x0;
            recon.extend_from_slice(&self.recon[plane][at..at + w]);
        }
        // The coefficient contexts the block's transforms write: one cell per 4 samples of the
        // in-plane rectangle (the coded plane is `4 * ctx_cols` samples wide, so these stay in
        // the arrays).
        let (cols, rows) = (x0 / 4..(x0 + w) / 4, y0 / 4..(y0 + h) / 4);
        PlaneState {
            plane,
            x0,
            y0,
            w,
            h,
            recon,
            above_level: self.above_level[plane][cols.clone()].to_vec(),
            above_dc: self.above_dc[plane][cols].to_vec(),
            left_level: self.left_level[plane][rows.clone()].to_vec(),
            left_dc: self.left_dc[plane][rows].to_vec(),
            block_decoded: self.block_decoded.clone(),
            max_luma: (self.max_luma_w, self.max_luma_h),
        }
    }

    /// The squared error of the current reconstruction over the region `s` captured.
    pub(super) fn region_sse(&self, s: &PlaneState) -> u64 {
        let stride = self.geom[s.plane].coded_w;
        let mut region = Vec::with_capacity(s.recon.len());
        for y in s.y0..s.y0 + s.h {
            let at = y * stride + s.x0;
            region.extend_from_slice(&self.recon[s.plane][at..at + s.w]);
        }
        self.sse(s.plane, s.x0, s.y0, s.w.max(1), &region)
    }

    /// Puts back what [`Self::save_plane`] captured.
    pub(super) fn restore_plane(&mut self, s: PlaneState) {
        let p = s.plane;
        // `s.w` is never 0: a block's top-left MI lies inside the frame, so `x0 < coded_w` on
        // every plane the frame codes, and `rw` is at least 4.
        self.store_recon(p, s.x0, s.y0, s.w, &s.recon);
        let (x4, y4) = (s.x0 / 4, s.y0 / 4);
        self.above_level[p][x4..x4 + s.above_level.len()].copy_from_slice(&s.above_level);
        self.above_dc[p][x4..x4 + s.above_dc.len()].copy_from_slice(&s.above_dc);
        self.left_level[p][y4..y4 + s.left_level.len()].copy_from_slice(&s.left_level);
        self.left_dc[p][y4..y4 + s.left_dc.len()].copy_from_slice(&s.left_dc);
        self.block_decoded = s.block_decoded;
        (self.max_luma_w, self.max_luma_h) = s.max_luma;
    }

    /// Predicts, codes and reconstructs one transform block. Under [`Coding::Trial`] nothing is
    /// coded: the block is reconstructed and its contexts updated exactly as [`Coding::Emit`]
    /// would, and the returned `(SSE, rate)` prices it — the rate of its coefficients (with the
    /// transform type) against the CDFs as they stand, in [`RATE_ONE`] units.
    #[allow(clippy::too_many_arguments)]
    fn transform_block(
        &mut self,
        plane: usize,
        sx: usize,
        sy: usize,
        block_w: usize,
        block_h: usize,
        tx_size: TxSize,
        desc: Pred,
        skip: bool,
        coding: Coding,
    ) -> (u64, u32) {
        // Lossless: forced 4×4 Walsh–Hadamard, DC prediction, source-as-recon (M0 path, unchanged).
        // Nothing is searched on this path, so it is never trialled.
        // Every lossless block is `DC_PRED` on every plane (`encode_block` takes `DC_LUMA` there
        // and chroma is plain DC), so its prediction is one value, subtracted here as such: a 4×4
        // array of it made any index into it equivalent to any other (a `pred[i + 4 + j]`
        // survived), so the per-sample index and its operator mutants are gone with the array.
        if self.qindex == 0 {
            debug_assert_eq!(desc.mode, DC_PRED, "lossless blocks are DC_PRED");
            let dc = self.dc_avg(plane, sx, sy);
            let mut res = [0i32; 16];
            for i in 0..4 {
                for j in 0..4 {
                    res[i * 4 + j] = self.sample(plane, sx + j, sy + i) - dc;
                }
            }
            let quant = gamut_dsp::av1::forward_wht4x4(&res);
            self.code_coeffs(
                plane,
                sx >> 2,
                sy >> 2,
                block_w,
                block_h,
                TxSize::Tx4x4,
                &quant,
                1,
                0,
            );
            return (0, 0);
        }

        // Lossy, square transform of side `n` (4 or 8). Predict from the reconstruction buffer,
        // forward-transform + quantize, code, then reconstruct exactly as the decoder will and store
        // back. Because the recon is `pred + inverse(dequant(levels))` and the decoder runs the same
        // inverse on the same levels, the result is bit-exact for whichever transform type is signaled.
        let (tw, th) = (tx_size.width(), tx_size.height());
        if plane == 0 {
            // §5.11.35: recorded for every in-bounds luma transform block, so after the plane-0
            // pass these hold the bottom-right edge CfL must clamp against.
            self.max_luma_w = sx + tw;
            self.max_luma_h = sy + th;
        }
        // Filter-intra is luma-only (§5.11.24), and every `Pred` a chroma plane receives is built
        // with `filter_intra: None`, so it needs no `plane == 0` guard here: one was equivalent
        // to `true` and is gone, with its two killable siblings.
        let pred: Vec<i32> = match desc.filter_intra {
            Some(fi) => self.predict_filter_intra(plane, sx, sy, tw, fi),
            _ => {
                let mut p = self.predict_intra(plane, sx, sy, desc.mode, tw, th, desc.angle_delta);
                if let Some(alpha) = desc.cfl_alpha {
                    self.apply_cfl(&mut p, sx, sy, alpha, tx_size);
                }
                p
            }
        };
        // skip = 1: no residual is coded; the reconstruction is the (clipped) prediction, and the
        // neighbour level/dc contexts are reset to 0 (`reset_block_context`, §5.11.10).
        if skip {
            let recon: Vec<u16> = pred
                .iter()
                .map(|&p| clip_pixel(p, self.bit_depth))
                .collect();
            let sse = self.sse(plane, sx, sy, tw, &recon);
            self.store_recon(plane, sx, sy, tw, &recon);
            self.set_ctx(plane, sx >> 2, sy >> 2, tw / 4, th / 4, 0, 0);
            return (sse, 0);
        }
        let mut res = vec![0i32; tw * th];
        for i in 0..th {
            for j in 0..tw {
                res[i * tw + j] = self.sample(plane, sx + j, sy + i) - pred[i * tw + j];
            }
        }
        // intraDir (§9.4) keys the transform-type CDF: the filter-intra mode's mapped direction
        // when filter-intra is used, else YMode. Unused for chroma (no transform type is coded).
        let intra_dir = match desc.filter_intra {
            Some(fi) => usize::from(FILTER_INTRA_MODE_TO_INTRA_DIR[fi as usize]),
            None => usize::from(desc.mode),
        };
        let site = TxSite {
            plane,
            sx,
            sy,
            block_w,
            block_h,
            tx_size,
            intra_dir,
        };
        let choice = self.select_tx_type(&site, &pred, &res, coding != Coding::Screen);
        match coding {
            Coding::Emit => self.code_coeffs(
                plane,
                sx >> 2,
                sy >> 2,
                block_w,
                block_h,
                tx_size,
                &choice.levels,
                choice.symbol,
                intra_dir,
            ),
            Coding::Trial | Coding::Screen => {
                self.set_ctx(
                    plane,
                    sx >> 2,
                    sy >> 2,
                    tw / 4,
                    th / 4,
                    choice.cul,
                    choice.dc,
                );
            }
        }
        self.store_recon(plane, sx, sy, tw, &choice.recon);
        (choice.sse, choice.rate)
    }

    /// The reconstruction a decoder produces for `levels` coded under `tx` over `pred`: dequantize,
    /// inverse-transform, add, clip (§7.12.3, §7.13).
    fn reconstruct(&self, pred: &[i32], levels: &[i32], tx_size: TxSize, tx: TxType) -> Vec<u16> {
        let (tw, th) = (tx_size.width(), tx_size.height());
        // dqDenom (§7.12.3) divides the dequantized coefficient by the transform size class. A
        // conformant decoder applies it, so the encoder reconstruction must.
        let dq_denom = tx_size.dq_denom();
        let mut dq = vec![0i32; tw * th];
        // The test `quantize_tx` compacts by, so the two layouts agree by construction. TX_64X64 is
        // the only transform with a 64-sample side this encoder forms (luma rectangles stop at 32,
        // and `chroma_tx_size` caps chroma at 32), so this is that size's test; a `tw == 64 &&
        // th == 64` here was equal to `||` on every transform that reaches it.
        if tw == 64 {
            // Re-expand the 32-stride coded levels into the top-left 32×32 of the 64-stride array the
            // inverse transform reads (the remaining coefficients stay zero).
            for i in 0..32 {
                for j in 0..32 {
                    let q = if i == 0 && j == 0 {
                        self.dc_quant
                    } else {
                        self.ac_quant
                    };
                    dq[i * 64 + j] = dequant(levels[i * 32 + j], q, dq_denom, self.bit_depth);
                }
            }
        } else {
            for (i, &lvl) in levels.iter().enumerate() {
                let q = if i == 0 { self.dc_quant } else { self.ac_quant };
                dq[i] = dequant(lvl, q, dq_denom, self.bit_depth);
            }
        }
        let resid = inverse_transform_2d(&dq, tx_size, tx, self.bit_depth);
        pred.iter()
            .zip(&resid)
            .map(|(&p, &r)| clip_pixel(p + r, self.bit_depth))
            .collect()
    }

    /// Writes a `w`-wide block of reconstructed samples into `plane` at coded `(sx, sy)`.
    fn store_recon(&mut self, plane: usize, sx: usize, sy: usize, w: usize, recon: &[u16]) {
        let stride = self.geom[plane].coded_w;
        for (i, row) in recon.chunks_exact(w).enumerate() {
            let at = (sy + i) * stride + sx;
            self.recon[plane][at..at + w].copy_from_slice(row);
        }
    }

    /// The sum of squared differences between a `w`-wide block of reconstructed samples and the
    /// (edge-replicated) source of `plane` at coded `(sx, sy)`: the distortion every
    /// rate–distortion decision weighs.
    fn sse(&self, plane: usize, sx: usize, sy: usize, w: usize, recon: &[u16]) -> u64 {
        let mut sse = 0u64;
        for (i, row) in recon.chunks_exact(w).enumerate() {
            for (j, &v) in row.iter().enumerate() {
                let d = i64::from(self.sample(plane, sx + j, sy + i)) - i64::from(v);
                sse += (d * d) as u64;
            }
        }
        sse
    }

    /// The rate–distortion cost `D + λ·R` of `sse` and `rate` (in [`RATE_ONE`] units), scaled to
    /// an integer.
    ///
    /// `λ = q² / LAMBDA_DIV` with `q` the frame's AC quantizer step (`base_q_idx`): high-rate
    /// quantization theory puts the slope of the distortion–rate curve at a fixed multiple of the
    /// squared step, and the step tables already carry the bit depth (a 10-bit step is 4× the
    /// 8-bit one, as a 10-bit error is 4× the 8-bit error), so the one multiple serves every depth.
    /// One `λ` for the whole frame, rather than one per block quantizer, so that a partition
    /// decision whose alternatives fall in different segments weighs them on one scale.
    pub(super) fn rd_cost(&self, sse: u64, rate: u32) -> u64 {
        let q = u64::from(ac_q(self.bit_depth, i32::from(self.qindex)).unsigned_abs());
        sse * u64::from(RATE_ONE) * LAMBDA_DIV + u64::from(rate) * q * q
    }

    /// Forward-transforms and quantizes a square residual under transform type `tx`, returning the
    /// quantized coefficient levels (DC uses `dc_quant`, AC uses `ac_quant`). Coefficients are
    /// pre-scaled by `dqDenom` so that `dequant(level, q, dqDenom)` recovers the coefficient (the
    /// decoder divides by `dqDenom` for 32×32; §7.12.3) — without it a 32×32 block would reconstruct
    /// at half residual amplitude. This is a quality choice; bit-exactness is unaffected (both the
    /// encoder reconstruction and the decoder dequantize the same coded levels with the same divisor).
    fn quantize_tx(&self, res: &[i32], tx_size: TxSize, tx: TxType) -> Vec<i32> {
        let coeff = forward_transform_2d(res, tx_size, tx);
        let denom = tx_size.dq_denom();
        if tx_size.width() == 64 {
            // TX_64X64 codes only its top-left 32×32 sub-block; compact those into the 32-stride
            // 1024-coefficient layout the coding/scan expect (the rest are zeroed by the transform).
            let mut levels = vec![0i32; 32 * 32];
            for i in 0..32 {
                for j in 0..32 {
                    let q = if i == 0 && j == 0 {
                        self.dc_quant
                    } else {
                        self.ac_quant
                    };
                    levels[i * 32 + j] = quantize(coeff[i * 64 + j] * denom, q);
                }
            }
            return levels;
        }
        let mut levels = vec![0i32; coeff.len()];
        for (i, &c) in coeff.iter().enumerate() {
            let q = if i == 0 { self.dc_quant } else { self.ac_quant };
            levels[i] = quantize(c * denom, q);
        }
        levels
    }

    /// Selects the transform type for the residual `res` of the transform block at `site` by
    /// rate–distortion cost: each candidate is quantized, priced against the current coefficient
    /// CDFs (the `intra_tx_type` symbol included), reconstructed over `pred` exactly as a decoder
    /// will, and weighed by [`Self::rd_cost`]. `DCT_DCT` is tried first and wins ties.
    ///
    /// Only a square luma transform up to 16×16 has a choice: the search's prefix of the rest of
    /// `TX_SET_INTRA_2`, unless `all_types` is false (a [`Coding::Screen`] trial). A 32×32 or larger
    /// luma block is `TX_SET_DCTONLY` (§5.11.48), and chroma and the rectangular transforms are
    /// coded `DCT_DCT` here, so they price that one candidate. The choice is a quality decision
    /// (any valid type round-trips bit-exactly); only the *signaling* must be correct.
    fn select_tx_type(
        &self,
        site: &TxSite,
        pred: &[i32],
        res: &[i32],
        all_types: bool,
    ) -> TxChoice {
        let (tw, th) = (site.tx_size.width(), site.tx_size.height());
        let trials = if all_types && site.plane == 0 && tw == th && tw < 32 {
            &TX_TYPE_TRIALS[..self.search.tx_type_trials]
        } else {
            &[]
        };
        let price = |tx: TxType, symbol: usize| -> (u64, TxChoice) {
            let levels = self.quantize_tx(res, site.tx_size, tx);
            let (rate, cul, dc) = self.coeff_rate(
                site.plane,
                site.sx >> 2,
                site.sy >> 2,
                site.block_w,
                site.block_h,
                site.tx_size,
                &levels,
                symbol,
                site.intra_dir,
            );
            let recon = self.reconstruct(pred, &levels, site.tx_size, tx);
            let sse = self.sse(site.plane, site.sx, site.sy, tw, &recon);
            let choice = TxChoice {
                symbol,
                levels,
                recon,
                sse,
                rate,
                cul,
                dc,
            };
            (self.rd_cost(sse, rate), choice)
        };
        // DCT_DCT (symbol 1) first, so it wins ties (`min_by_key` keeps its first argument on
        // equal keys); the rest of TX_SET_INTRA_2 follows in trial order.
        trials
            .iter()
            .fold(price(TxType::DctDct, 1), |best, &(tx, symbol)| {
                std::cmp::min_by_key(best, price(tx, symbol), |c| c.0)
            })
            .1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Effort;
    use crate::tile::tests::{at_effort, grey4x4, planes_420};
    /// `select_tx_type`'s choice for the luma 4x4 at the origin of a mid-grey frame at `qindex`
    /// whose residual is `res` (the prediction is the source minus `res`).
    fn pick_tx(qindex: u8, effort: Effort, res: &[i32; 16]) -> TxChoice {
        let p = grey4x4();
        let e = at_effort(&p, qindex, effort);
        let pred: Vec<i32> = res.iter().map(|&r| 128 - r).collect();
        let site = TxSite {
            plane: 0,
            sx: 0,
            sy: 0,
            block_w: 4,
            block_h: 4,
            tx_size: TxSize::Tx4x4,
            intra_dir: usize::from(DC_PRED),
        };
        e.select_tx_type(&site, &pred, res, true)
    }

    #[test]
    fn select_tx_type_tries_only_the_preset_types() {
        // A lone non-DC impulse is the residual IDTX codes best (see
        // `tx_type_selection_matches_set2_mapping`). `Fast` tries DCT_DCT and ADST_ADST only, and
        // `Fastest` DCT_DCT alone, so neither may answer IDTX (symbol 0).
        let mut impulse = [0i32; 16];
        impulse[5] = 220;
        assert_eq!(pick_tx(48, Effort::Thorough, &impulse).symbol, 0);
        assert!([1, 2].contains(&pick_tx(48, Effort::Fast, &impulse).symbol));
        assert_eq!(pick_tx(48, Effort::Fastest, &impulse).symbol, 1);
        // A screening trial prices DCT_DCT alone even under the full search.
        let p = grey4x4();
        let e = at_effort(&p, 48, Effort::Thorough);
        let pred: Vec<i32> = impulse.iter().map(|&r| 128 - r).collect();
        let site = TxSite {
            plane: 0,
            sx: 0,
            sy: 0,
            block_w: 4,
            block_h: 4,
            tx_size: TxSize::Tx4x4,
            intra_dir: usize::from(DC_PRED),
        };
        assert_eq!(e.select_tx_type(&site, &pred, &impulse, false).symbol, 1);
    }

    #[test]
    fn rd_cost_weighs_rate_by_the_frames_squared_ac_step() {
        let p = grey4x4();
        let mut e = FrameEncoder::new(&p, 40);
        let q = u64::from(ac_q(8, 40).unsigned_abs());
        assert_eq!(e.rd_cost(3, 0), 3 * u64::from(RATE_ONE) * LAMBDA_DIV);
        assert_eq!(e.rd_cost(0, 5 * RATE_ONE), 5 * u64::from(RATE_ONE) * q * q);
        // A segment's quantizer does not move λ.
        e.apply_seg_quant(2);
        assert_eq!(e.rd_cost(0, RATE_ONE), u64::from(RATE_ONE) * q * q);
    }

    #[test]
    fn chroma_tx_is_the_block_transform_on_the_lossy_path_and_4x4_otherwise() {
        let p = grey4x4();
        let lossy = FrameEncoder::new(&p, 40);
        assert_eq!(lossy.chroma_tx(8, 8), TxSize::Tx8x8);
        assert_eq!(lossy.chroma_tx(16, 8), TxSize::Tx16x8);
        assert_eq!(lossy.chroma_tx(64, 64), TxSize::Tx32x32);
        assert_eq!(lossy.chroma_tx(4, 4), TxSize::Tx4x4);
        let lossless = FrameEncoder::new(&p, 0);
        assert_eq!(lossless.chroma_tx(8, 8), TxSize::Tx4x4);
        let sub = planes_420(16, 16);
        assert_eq!(FrameEncoder::new(&sub, 40).chroma_tx(16, 16), TxSize::Tx8x8);
    }

    #[test]
    fn chroma_transform_size_caps_a_sixty_four_sample_axis() {
        // §5.11.37: chroma never uses a 64-sample transform, and the cap applies when *either*
        // axis reaches 64 — an `&&` would leave a 32x64 residual on TX_32X64.
        assert_eq!(chroma_tx_size(32, 64), TxSize::Tx32x32);
        assert_eq!(chroma_tx_size(64, 32), TxSize::Tx32x32);
        assert_eq!(chroma_tx_size(64, 64), TxSize::Tx32x32);
        assert_eq!(chroma_tx_size(16, 64), TxSize::Tx16x32);
        assert_eq!(chroma_tx_size(64, 16), TxSize::Tx32x16);
        // Below 64 the transform matches the residual exactly.
        assert_eq!(chroma_tx_size(32, 32), TxSize::Tx32x32);
        assert_eq!(chroma_tx_size(4, 8), TxSize::Tx4x8);
        assert_eq!(chroma_tx_size(8, 4), TxSize::Tx8x4);
    }

    #[test]
    fn max_tx_size_rect_matches_the_block_shape() {
        // §9.3 `Max_Tx_Size_Rect`: the transform has the block's own dimensions. Every shape is
        // pinned, because the previous partial table silently fell back to `square_tx(w)` — which
        // is the wrong transform for any pair it did not list, and would have become reachable the
        // moment subsampling produced one.
        let cases = [
            ((4, 4), TxSize::Tx4x4),
            ((4, 8), TxSize::Tx4x8),
            ((4, 16), TxSize::Tx4x16),
            ((8, 4), TxSize::Tx8x4),
            ((8, 8), TxSize::Tx8x8),
            ((8, 16), TxSize::Tx8x16),
            ((8, 32), TxSize::Tx8x32),
            ((16, 4), TxSize::Tx16x4),
            ((16, 8), TxSize::Tx16x8),
            ((16, 16), TxSize::Tx16x16),
            ((16, 32), TxSize::Tx16x32),
            ((16, 64), TxSize::Tx16x64),
            ((32, 8), TxSize::Tx32x8),
            ((32, 16), TxSize::Tx32x16),
            ((32, 32), TxSize::Tx32x32),
            ((32, 64), TxSize::Tx32x64),
            ((64, 16), TxSize::Tx64x16),
            ((64, 32), TxSize::Tx64x32),
            ((64, 64), TxSize::Tx64x64),
        ];
        for ((w, h), want) in cases {
            assert_eq!(max_tx_size_rect(w, h), want, "{w}x{h}");
        }
        // The 128-sample axes clamp to 64, so the three 128-wide blocks share TX_64X64 with the
        // 64x64 one rather than naming a transform that does not exist.
        assert_eq!(max_tx_size_rect(64, 128), TxSize::Tx64x64);
        assert_eq!(max_tx_size_rect(128, 64), TxSize::Tx64x64);
        assert_eq!(max_tx_size_rect(128, 128), TxSize::Tx64x64);
    }

    #[test]
    fn every_transform_the_encoder_selects_has_an_eob_position_cdf() {
        // The `eob_pt` selector panics rather than falling through to the 1024-coefficient table,
        // so this asserts the selector is total over the transforms `max_tx_size_rect` can return.
        // 32 is the entry this change adds: it is unreachable at 4:4:4 and becomes reachable as
        // soon as chroma is subsampled (TX_4X8 / TX_8X4).
        let areas: std::collections::BTreeSet<usize> = [
            (4, 4),
            (4, 8),
            (4, 16),
            (8, 4),
            (8, 8),
            (8, 16),
            (8, 32),
            (16, 4),
            (16, 8),
            (16, 16),
            (16, 32),
            (16, 64),
            (32, 8),
            (32, 16),
            (32, 32),
            (32, 64),
            (64, 16),
            (64, 32),
            (64, 64),
        ]
        .iter()
        .map(|&(w, h)| {
            let tx = max_tx_size_rect(w, h);
            // The coded coefficient count is capped at 1024 (§5.11.39 `segEob`).
            (tx.width() * tx.height()).min(1024)
        })
        .collect();
        assert_eq!(
            areas,
            [16, 32, 64, 128, 256, 512, 1024].into_iter().collect(),
            "the eob_pt selector must cover exactly these coefficient counts"
        );
    }

    #[test]
    fn tx_type_selection_matches_set2_mapping() {
        // A flat residual concentrates into the DCT DC bin (one coefficient), so DCT_DCT wins and
        // is signaled as Tx_Type_Intra_Inv_Set2 symbol 1.
        assert_eq!(pick_tx(48, Effort::Thorough, &[40; 16]).symbol, 1);

        // A single non-DC impulse stays a single coefficient under the identity transform but
        // spreads across many DCT bins, so IDTX wins and is signaled as symbol 0.
        let mut impulse = [0i32; 16];
        impulse[5] = 220;
        let idtx = pick_tx(48, Effort::Thorough, &impulse);
        assert_eq!(idtx.symbol, 0);
        assert_eq!(idtx.levels.iter().filter(|&&l| l != 0).count(), 1);
    }

    #[test]
    fn tx_type_selection_takes_the_cheapest_candidate_and_reports_it_faithfully() {
        // `select_tx_type` is the one place a transform type is weighed, so its answer must be
        // the candidate of least `rd_cost`, and everything it returns about that candidate must be
        // what coding it produces: the levels the quantizer gives, the reconstruction a decoder
        // makes of them, that reconstruction's squared error, and the coefficient rate.
        let p = grey4x4();
        let e = FrameEncoder::new(&p, 90);
        let site = TxSite {
            plane: 0,
            sx: 0,
            sy: 0,
            block_w: 4,
            block_h: 4,
            tx_size: TxSize::Tx4x4,
            intra_dir: usize::from(DC_PRED),
        };
        for res in [
            std::array::from_fn::<i32, 16, _>(|i| (i as i32 - 8) * 17),
            std::array::from_fn(|i| if i % 4 < 2 { 60 } else { -60 }),
            std::array::from_fn(|i| [90, -30, 0, 45][i % 4] + [0, 20, -20, 0][i / 4]),
        ] {
            let pred: Vec<i32> = res.iter().map(|&r| 128 - r).collect();
            let choice = e.select_tx_type(&site, &pred, &res, true);
            let cost_of = |tx: TxType, symbol: usize| {
                let levels = e.quantize_tx(&res, TxSize::Tx4x4, tx);
                let (rate, ..) = e.coeff_rate(
                    0,
                    0,
                    0,
                    4,
                    4,
                    TxSize::Tx4x4,
                    &levels,
                    symbol,
                    site.intra_dir,
                );
                let recon = e.reconstruct(&pred, &levels, TxSize::Tx4x4, tx);
                (
                    e.rd_cost(e.sse(0, 0, 0, 4, &recon), rate),
                    levels,
                    recon,
                    rate,
                )
            };
            let all = [(TxType::DctDct, 1)].into_iter().chain(TX_TYPE_TRIALS);
            let costs: Vec<(usize, u64)> =
                all.clone().map(|(tx, s)| (s, cost_of(tx, s).0)).collect();
            let least = costs.iter().map(|c| c.1).min().unwrap();
            let first_least = costs.iter().find(|c| c.1 == least).unwrap().0;
            assert_eq!(choice.symbol, first_least, "{res:?}: {costs:?}");
            let (tx, _) = all.clone().find(|&(_, s)| s == choice.symbol).unwrap();
            let (_, levels, recon, rate) = cost_of(tx, choice.symbol);
            assert_eq!(choice.levels, levels);
            assert_eq!(choice.recon, recon);
            assert_eq!(choice.sse, e.sse(0, 0, 0, 4, &recon));
            assert_eq!(choice.rate, rate);
        }
    }
}
