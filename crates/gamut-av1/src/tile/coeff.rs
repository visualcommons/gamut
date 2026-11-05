//! Coefficient coding (§5.11.39): the syntax walk over one transform block's quantized levels,
//! fed to a [`CoeffSink`] that either codes or prices it, and the §8.3.2 neighbour contexts it is
//! coded under.

use std::sync::LazyLock;

use super::FrameEncoder;
use super::coder::{Coder, RATE_ONE, symbol_rate};
use crate::cdf;
use crate::transform::TxSize;

/// `NUM_BASE_LEVELS` (§3).
const NUM_BASE_LEVELS: i32 = 2;
/// `NUM_BASE_LEVELS + COEFF_BASE_RANGE`, the golomb threshold (§5.11.39).
const COEFF_BASE_PLUS_RANGE: i32 = 14;

/// `Default_Scan` (§9.2) for every coded coefficient region, `SCANS[i][j]` being `4 << i` wide by
/// `4 << j` high (a 64-sample transform dimension codes a 32-sample region, §7.13), built once
/// from [`cdf::default_scan`]. One table for squares and rectangles alike: the hand-transcribed
/// square tables (`cdf::DEFAULT_SCAN_*`) are what `default_scan` is tested against, and a square
/// shortcut beside the generator was a choice between two equal answers — four match arms whose
/// deletion no input could see.
static SCANS: LazyLock<[[Vec<usize>; 4]; 4]> = LazyLock::new(|| {
    std::array::from_fn(|i| std::array::from_fn(|j| cdf::default_scan(4 << i, 4 << j)))
});

/// The CDF a coefficient-coding symbol (§5.11.39) is coded under, by its indices into
/// [`cdf::CdfContext`]. Naming the CDF rather than borrowing it lets one walk of the syntax either
/// code the symbols or only price them.
#[derive(Clone, Copy)]
enum CoeffCdf {
    /// `all_zero`: `[txSzCtx][ctx]`.
    TxbSkip(usize, usize),
    /// `intra_tx_type` under `TX_SET_INTRA_2`: `[Tx_Size_Sqr][intraDir]`.
    TxType(usize, usize),
    /// `eob_pt_*`: the [`eob_pt_class`] of the coded area, then `[ptype]`.
    EobPt(usize, usize),
    /// `eob_extra`: `[txSzCtx][ptype][eobPt - 3]`.
    EobExtra(usize, usize, usize),
    /// `coeff_base_eob`: `[txSzCtx][ptype][ctx]`.
    BaseEob(usize, usize, usize),
    /// `coeff_base`: `[txSzCtx][ptype][ctx]`.
    Base(usize, usize, usize),
    /// `coeff_br`: `[Min(txSzCtx, 3)][ptype][ctx]`.
    Br(usize, usize, usize),
    /// `dc_sign`: `[ptype][ctx]`.
    DcSign(usize, usize),
}

impl CoeffCdf {
    /// The adapting CDF this names, for coding a symbol against it.
    fn slot(self, cdfs: &mut cdf::CdfContext) -> cdf::CdfSlot<'_> {
        match self {
            Self::TxbSkip(sz, ctx) => cdfs.txb_skip[sz][ctx].slot(),
            Self::TxType(sqr, dir) => cdfs.intra_tx_type_set2[sqr][dir].slot(),
            Self::EobPt(class, ptype) => match class {
                0 => cdfs.eob_pt_16[ptype].slot(),
                1 => cdfs.eob_pt_32[ptype].slot(),
                2 => cdfs.eob_pt_64[ptype].slot(),
                3 => cdfs.eob_pt_128[ptype].slot(),
                4 => cdfs.eob_pt_256[ptype].slot(),
                5 => cdfs.eob_pt_512[ptype].slot(),
                _ => cdfs.eob_pt_1024[ptype].slot(),
            },
            Self::EobExtra(sz, ptype, i) => cdfs.eob_extra[sz][ptype][i].slot(),
            Self::BaseEob(sz, ptype, ctx) => cdfs.coeff_base_eob[sz][ptype][ctx].slot(),
            Self::Base(sz, ptype, ctx) => cdfs.coeff_base[sz][ptype][ctx].slot(),
            Self::Br(sz, ptype, ctx) => cdfs.coeff_br[sz][ptype][ctx].slot(),
            Self::DcSign(ptype, ctx) => cdfs.dc_sign[ptype][ctx].slot(),
        }
    }

    /// The current probabilities of the CDF this names, for pricing a symbol without coding it.
    fn probs(self, cdfs: &cdf::CdfContext) -> &[u16] {
        match self {
            Self::TxbSkip(sz, ctx) => cdfs.txb_skip[sz][ctx].probs(),
            Self::TxType(sqr, dir) => cdfs.intra_tx_type_set2[sqr][dir].probs(),
            Self::EobPt(class, ptype) => match class {
                0 => cdfs.eob_pt_16[ptype].probs(),
                1 => cdfs.eob_pt_32[ptype].probs(),
                2 => cdfs.eob_pt_64[ptype].probs(),
                3 => cdfs.eob_pt_128[ptype].probs(),
                4 => cdfs.eob_pt_256[ptype].probs(),
                5 => cdfs.eob_pt_512[ptype].probs(),
                _ => cdfs.eob_pt_1024[ptype].probs(),
            },
            Self::EobExtra(sz, ptype, i) => cdfs.eob_extra[sz][ptype][i].probs(),
            Self::BaseEob(sz, ptype, ctx) => cdfs.coeff_base_eob[sz][ptype][ctx].probs(),
            Self::Base(sz, ptype, ctx) => cdfs.coeff_base[sz][ptype][ctx].probs(),
            Self::Br(sz, ptype, ctx) => cdfs.coeff_br[sz][ptype][ctx].probs(),
            Self::DcSign(ptype, ctx) => cdfs.dc_sign[ptype][ctx].probs(),
        }
    }
}

/// The `eob_pt_*` table (§5.11.39) for transform `tx`, as the index [`CoeffCdf::EobPt`] carries:
/// the coded coefficient count — the transform's area with each axis capped at 32 (§7.13), so
/// 16, 32, 64, 128, 256, 512 or 1024 — maps to `log2(count) - 4`, `0..=6`.
///
/// Taking the [`TxSize`] rather than a coefficient count makes the function total: every one of
/// the 19 transform sizes has a capped area in that set, so no count without a table of its own
/// can reach the coder.
fn eob_pt_class(tx: TxSize) -> usize {
    (tx.log2_width().min(5) + tx.log2_height().min(5) - 4) as usize
}

/// Where the coefficient syntax walk ([`FrameEncoder::walk_coeffs`]) sends each symbol: into the
/// tile's coder ([`Emit`]) or into a rate estimate against borrowed CDFs ([`Tally`]).
trait CoeffSink {
    /// One adaptive-CDF symbol.
    fn symbol(&mut self, symbol: usize, cdf: CoeffCdf);
    /// The low `n` bits of `value` as equiprobable bits.
    fn literal(&mut self, value: u32, n: u32);
}

/// Codes the symbols, adapting the CDFs as a decoder will.
struct Emit<'s> {
    sym: &'s mut Coder,
    cdfs: &'s mut cdf::CdfContext,
}

impl CoeffSink for Emit<'_> {
    fn symbol(&mut self, symbol: usize, cdf: CoeffCdf) {
        self.sym.encode(symbol, cdf.slot(self.cdfs));
    }

    fn literal(&mut self, value: u32, n: u32) {
        self.sym.encode_literal(value, n);
    }
}

/// Sums the symbols' rate against the CDFs as they stand, without adapting them.
struct Tally<'s> {
    cdfs: &'s cdf::CdfContext,
    rate: u32,
}

impl CoeffSink for Tally<'_> {
    fn symbol(&mut self, symbol: usize, cdf: CoeffCdf) {
        self.rate += symbol_rate(cdf.probs(self.cdfs), symbol);
    }

    fn literal(&mut self, _value: u32, n: u32) {
        self.rate += n * RATE_ONE;
    }
}

/// The neighbour-derived contexts one transform block's coefficient syntax is coded under.
struct CoeffJob {
    /// `ptype`: 0 for luma, 1 for chroma.
    ptype: usize,
    tx_size: TxSize,
    /// The `all_zero` context.
    txb_ctx: usize,
    /// The `dc_sign` context.
    dc_ctx: usize,
    /// `(Tx_Size_Sqr, intraDir, symbol)` when an `intra_tx_type` is coded, else `None`.
    tx_type: Option<(usize, usize, usize)>,
}

/// `eobPt` from `eob` (inverts `eob = (eobPt < 2) ? eobPt : (1 << (eobPt-2)) + 1`, §5.11.39).
fn eobpt_from_eob(eob: usize) -> usize {
    if eob <= 1 {
        eob
    } else {
        (32 - ((eob - 1) as u32).leading_zeros()) as usize + 1
    }
}

/// `get_coeff_base_ctx` for the end-of-block coefficient (§8.3.2), `isEob == 1`, for a square
/// `TX_CLASS_2D` block of `area` (`= width * height`) coefficients. Maps the scan index `c` to one
/// of the 4 `coeff_base_eob` contexts.
fn coeff_base_eob_ctx(c: usize, area: usize) -> usize {
    if c == 0 {
        0
    } else if c <= area / 8 {
        1
    } else if c <= area / 4 {
        2
    } else {
        3
    }
}

/// `get_coeff_base_ctx` for a non-EOB coefficient (§8.3.2) of a square `TX_CLASS_2D` block: `bwl` is
/// `Tx_Width_Log2`, `n = 1 << bwl` is the (square) transform side, and `offset` is the
/// `Coeff_Base_Ctx_Offset[txSz]` table. `levels` holds the magnitudes already decoded along the
/// reverse scan, indexed by the linear coefficient position.
fn coeff_base_ctx(
    pos: usize,
    levels: &[i32],
    bwl: usize,
    w: usize,
    h: usize,
    offset: &[[u8; 5]; 5],
) -> usize {
    let (row, col) = (pos >> bwl, pos & (w - 1));
    let mut mag = 0i32;
    for &(dr, dc) in &cdf::SIG_REF_DIFF_OFFSET_2D {
        let (rr, cc) = (row + dr, col + dc);
        if rr < h && cc < w {
            mag += levels[(rr << bwl) + cc].abs().min(3);
        }
    }
    let ctx = (((mag + 1) >> 1).min(4)) as usize;
    if row == 0 && col == 0 {
        return 0;
    }
    ctx + usize::from(offset[row.min(4)][col.min(4)])
}

/// `get_coeff_br_ctx` for a `TX_CLASS_2D` block (§8.3.2): `bwl = Tx_Width_Log2`; the coded region is
/// `w × h` (`w == h` for a square transform, the top-left `32 × 32` for `TX_64X64`).
fn coeff_br_ctx(pos: usize, levels: &[i32], bwl: usize, w: usize, h: usize) -> usize {
    let (row, col) = (pos >> bwl, pos & (w - 1));
    let mut mag = 0i32;
    for &(dr, dc) in &cdf::MAG_REF_OFFSET_2D {
        let (rr, cc) = (row + dr, col + dc);
        if rr < h && cc < w {
            mag += levels[(rr << bwl) + cc].abs().min(15);
        }
    }
    let mag = (((mag + 1) >> 1).min(6)) as usize;
    if pos == 0 {
        mag
    } else if row < 2 && col < 2 {
        mag + 7
    } else {
        mag + 14
    }
}

/// Exp-Golomb tail used for coefficient magnitudes above the base-range cap (§5.11.39).
fn golomb(sink: &mut impl CoeffSink, x: u32) {
    let len = 32 - x.leading_zeros(); // bit length, x >= 1
    for _ in 0..(len - 1) {
        sink.literal(0, 1);
    }
    sink.literal(1, 1);
    let mut i = len as isize - 2;
    while i >= 0 {
        sink.literal((x >> i) & 1, 1);
        i -= 1;
    }
}

impl FrameEncoder<'_> {
    /// Codes one transform block's coefficients (§5.11.39) and updates the neighbour level/DC
    /// contexts it leaves behind.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn code_coeffs(
        &mut self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_w: usize,
        block_h: usize,
        tx_size: TxSize,
        quant: &[i32],
        tx_sym: usize,
        intra_dir: usize,
    ) {
        let job = self.coeff_job(plane, x4, y4, block_w, block_h, tx_size, tx_sym, intra_dir);
        let mut sink = Emit {
            sym: &mut self.sym,
            cdfs: &mut self.cdfs,
        };
        let (cul, dc_cat) = Self::walk_coeffs(&mut sink, &job, quant);
        let (w4, h4) = (tx_size.width() / 4, tx_size.height() / 4);
        self.set_ctx(plane, x4, y4, w4, h4, cul, dc_cat);
    }

    /// The rate, in [`RATE_ONE`] units, that [`Self::code_coeffs`] would spend on these
    /// coefficients under the current (unadapted) CDFs, with the `culLevel`/`dcCategory` the block
    /// would leave in the neighbour contexts.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn coeff_rate(
        &self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_w: usize,
        block_h: usize,
        tx_size: TxSize,
        quant: &[i32],
        tx_sym: usize,
        intra_dir: usize,
    ) -> (u32, u8, u8) {
        let job = self.coeff_job(plane, x4, y4, block_w, block_h, tx_size, tx_sym, intra_dir);
        let mut sink = Tally {
            cdfs: &self.cdfs,
            rate: 0,
        };
        let (cul, dc_cat) = Self::walk_coeffs(&mut sink, &job, quant);
        (sink.rate, cul, dc_cat)
    }

    /// Everything [`Self::walk_coeffs`] reads from the encoder's neighbour state, resolved up
    /// front so the walk can borrow the symbol coder and CDFs mutably.
    #[allow(clippy::too_many_arguments)]
    fn coeff_job(
        &self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_w: usize,
        block_h: usize,
        tx_size: TxSize,
        tx_sym: usize,
        intra_dir: usize,
    ) -> CoeffJob {
        let (w, h) = (tx_size.width(), tx_size.height());
        // transform_type (§5.11.39): signaled only for luma when not all-zero and base_q_idx > 0.
        // Reduced tx set + 4×4/8×8/16×16 intra ⇒ TX_SET_INTRA_2; `tx_sym` indexes
        // Tx_Type_Intra_Inv_Set2 = {IDTX, DCT_DCT, ADST_ADST, ADST_DCT, DCT_ADST}. The CDF is uniform
        // across intra direction for 4×4/8×8 but per-`intraDir` for 16×16 (§9.4). A 32×32 intra block
        // is TX_SET_DCTONLY (§5.11.48 `txSzSqrUp == TX_32X32`), so no transform type is signaled.
        // The set is TX_SET_INTRA_2 when `txSzSqrUp ≤ TX_16X16` (`up_sq ≤ 16`), else DCTONLY (no
        // type). The CDF is keyed by `txSzSqr = Min(w, h)`: uniform for ≤ 8, per-`intraDir` for 16×16.
        // `TileIntraTxTypeSet2Cdf[Tx_Size_Sqr[txSz]][intraDir]`: one context per (square size,
        // intra direction). The 4×4 and 8×8 defaults are the same uniform row for every
        // direction, but each pair still adapts on its own.
        let tx_type = (self.qindex > 0 && plane == 0 && w.max(h) <= 16)
            .then(|| (cdf::tx_sqr_class(w.min(h)), intra_dir, tx_sym));
        CoeffJob {
            ptype: usize::from(plane > 0),
            tx_size,
            txb_ctx: self.txb_skip_ctx(plane, x4, y4, block_w, block_h, w, h),
            dc_ctx: self.dc_sign_ctx(plane, x4, y4, w / 4, h / 4),
            tx_type,
        }
    }

    /// The §5.11.39 coefficient syntax for `quant`, fed symbol by symbol to `sink`. Returns the
    /// `culLevel` and `dcCategory` the block leaves in the neighbour contexts.
    #[allow(clippy::too_many_lines)]
    fn walk_coeffs(sink: &mut impl CoeffSink, job: &CoeffJob, quant: &[i32]) -> (u8, u8) {
        let ptype = job.ptype;
        let (w, h) = (job.tx_size.width(), job.tx_size.height());
        // A >32 dimension (TX_64X64) codes only its top-left 32-wide/high sub-block (§7.13): the scan,
        // area and coefficient contexts use the `code_w × code_h` region, while the CDF tables and
        // `txb_skip`/`set_ctx` span the full transform.
        let (code_w, code_h) = (w.min(32), h.min(32));
        let bwl = code_w.trailing_zeros() as usize;
        let area = code_w * code_h;
        // The rectangular transforms share their bounding square's coefficient CDFs (`txSzCtx` = that
        // square's ctx; the `coeff_br` Min(txSzCtx,3) cap is already baked into the 64 row). Only the
        // scan order and the eob class (by coefficient count) differ.
        let up_sq = w.max(h);

        let scan: &[usize] = &SCANS[bwl - 2][code_h.trailing_zeros() as usize - 2];

        // `txSzCtx` (§8.3.2): the bounding square's class, which keys the skip/eob/base CDFs.
        // `coeff_br` is capped at TX_32X32 (`Min(txSzCtx, 3)`), so TX_64X64 shares its row.
        let sz = match up_sq {
            4 => 0,
            8 => 1,
            16 => 2,
            32 => 3,
            _ => 4,
        };
        let br_sz = sz.min(3);
        let offset: &[[u8; 5]; 5] = if up_sq == 4 {
            &cdf::COEFF_BASE_CTX_OFFSET_4X4
        } else {
            &cdf::COEFF_BASE_CTX_OFFSET_8X8
        };

        // A rectangular transform uses an aspect-specific `coeff_base` offset table (§8.3.2): the
        // square table only fits `w == h`; wide and tall blocks differ at the low frequencies.
        let offset: &[[u8; 5]; 5] = if w > h {
            &cdf::COEFF_BASE_CTX_OFFSET_WIDE
        } else if w < h {
            &cdf::COEFF_BASE_CTX_OFFSET_TALL
        } else {
            offset
        };

        let mut eob = 0usize;
        for c in 0..area {
            if quant[scan[c]] != 0 {
                eob = c + 1;
            }
        }

        sink.symbol(usize::from(eob == 0), CoeffCdf::TxbSkip(sz, job.txb_ctx));
        if eob == 0 {
            return (0, 0);
        }

        if let Some((sqr, intra_dir, tx_sym)) = job.tx_type {
            sink.symbol(tx_sym, CoeffCdf::TxType(sqr, intra_dir));
        }

        // eob position (TX_CLASS_2D ⇒ eob_pt context 0). The eob class is the coded coefficient count
        // (`area`): 16/32/64/128/256/512/1024. The 512 and 1024 tables have no neighbour-context
        // dimension. 32 is reachable only with subsampled chroma (TX_4X8 / TX_8X4).
        let eobpt = eobpt_from_eob(eob);
        sink.symbol(eobpt - 1, CoeffCdf::EobPt(eob_pt_class(job.tx_size), ptype));
        if eobpt >= 3 {
            let nbits = eobpt - 2;
            // §5.11.39 codes `eob - (1 << nbits) - 1` in `nbits` bits. `eob - 1` lies in
            // `[1 << nbits, 2 << nbits)`, so it is that value with its top bit set, and the low
            // `nbits` bits — all that is coded — are the same. Subtracting the top bit as well
            // would be an operation no coded bit can see.
            let extra = eob - 1;
            sink.symbol(
                (extra >> (nbits - 1)) & 1,
                CoeffCdf::EobExtra(sz, ptype, eobpt - 3),
            );
            let mut i = nbits as isize - 2;
            while i >= 0 {
                sink.literal(((extra >> i) & 1) as u32, 1);
                i -= 1;
            }
        }

        // Base levels + base range, scanned from the last coefficient back to DC.
        let mut levels = vec![0i32; area];
        for c in (0..eob).rev() {
            let pos = scan[c];
            let level = quant[pos].abs();
            if c == eob - 1 {
                let ctx = coeff_base_eob_ctx(c, area);
                sink.symbol(
                    (level.min(3) - 1) as usize,
                    CoeffCdf::BaseEob(sz, ptype, ctx),
                );
            } else {
                let ctx = coeff_base_ctx(pos, &levels, bwl, code_w, code_h, offset);
                sink.symbol(level.min(3) as usize, CoeffCdf::Base(sz, ptype, ctx));
            }
            if level > NUM_BASE_LEVELS {
                let br_ctx = coeff_br_ctx(pos, &levels, bwl, code_w, code_h);
                let mut rem = level - 3;
                for _ in 0..4 {
                    let brv = rem.min(3);
                    sink.symbol(brv as usize, CoeffCdf::Br(br_sz, ptype, br_ctx));
                    rem -= brv;
                    if brv < 3 {
                        break;
                    }
                }
            }
            levels[pos] = level;
        }

        // Signs (DC sign is CDF-coded; the rest are raw bits) and golomb tails.
        for (c, &pos) in scan.iter().enumerate().take(eob) {
            let level = quant[pos].abs();
            if level != 0 {
                let neg = quant[pos].is_negative();
                if c == 0 {
                    sink.symbol(usize::from(neg), CoeffCdf::DcSign(ptype, job.dc_ctx));
                } else {
                    sink.literal(u32::from(neg), 1);
                }
                if level > COEFF_BASE_PLUS_RANGE {
                    golomb(sink, (level - COEFF_BASE_PLUS_RANGE) as u32);
                }
            }
        }

        let cul = levels.iter().sum::<i32>().min(63) as u8;
        // `dcCategory`: one three-way comparison, so no bound overlaps the zero case before it.
        let dc_cat = match quant[0].cmp(&0) {
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Less => 1,
            std::cmp::Ordering::Greater => 2,
        };
        (cul, dc_cat)
    }

    /// §8.3.2 `maxX4`: the entropy-context column count of `plane` — the luma MI grid scaled by
    /// the plane's subsampling. `mi_cols` is always even, so the shift is exact. Identical to
    /// `mi_cols` at 4:4:4.
    fn ctx_cols(&self, plane: usize) -> usize {
        self.mi_cols >> self.geom[plane].ss_x
    }

    /// §8.3.2 `maxY4`, the row counterpart of [`Self::ctx_cols`].
    fn ctx_rows(&self, plane: usize) -> usize {
        self.mi_rows >> self.geom[plane].ss_y
    }

    /// Writes `culLevel`/`dcCategory` into the above/left level-context arrays for every MI cell the
    /// transform block spans (§5.11.39: `for i in 0..w4`/`0..h4`). `n4 = Tx_Width / 4`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn set_ctx(
        &mut self,
        plane: usize,
        x4: usize,
        y4: usize,
        w4: usize,
        h4: usize,
        cul: u8,
        dc: u8,
    ) {
        let (cols, rows) = (self.ctx_cols(plane), self.ctx_rows(plane));
        for k in 0..w4 {
            if x4 + k < cols {
                self.above_level[plane][x4 + k] = cul;
                self.above_dc[plane][x4 + k] = dc;
            }
        }
        for k in 0..h4 {
            if y4 + k < rows {
                self.left_level[plane][y4 + k] = cul;
                self.left_dc[plane][y4 + k] = dc;
            }
        }
    }

    /// `all_zero` (txb_skip) context (§8.3.2). `block_w` is the residual-block width, `tx_w` the
    /// transform width: luma uses ctx 0 when the transform covers the whole block (the only case the
    /// lossy path reaches, since `TX_MODE_LARGEST` ⇒ tx == block); otherwise (lossless, where a
    /// larger block is split into 4×4 transforms) it falls back to the neighbour-level classification.
    #[allow(clippy::too_many_arguments)]
    fn txb_skip_ctx(
        &self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_w: usize,
        block_h: usize,
        tx_w: usize,
        tx_h: usize,
    ) -> usize {
        let (w4, h4) = (tx_w / 4, tx_h / 4);
        if plane == 0 {
            if block_w == tx_w && block_h == tx_h {
                return 0;
            }
            let mut top = 0i32;
            let mut left = 0i32;
            for k in 0..w4 {
                if x4 + k < self.ctx_cols(0) {
                    top = top.max(i32::from(self.above_level[0][x4 + k]));
                }
            }
            for k in 0..h4 {
                if y4 + k < self.ctx_rows(0) {
                    left = left.max(i32::from(self.left_level[0][y4 + k]));
                }
            }
            if top == 0 && left == 0 {
                1
            } else if top == 0 || left == 0 {
                2 + usize::from(top.max(left) > 3)
            } else if top.max(left) <= 3 {
                4
            } else if top.min(left) <= 3 {
                5
            } else {
                6
            }
        } else {
            let mut above = 0u8;
            let mut left = 0u8;
            for k in 0..w4 {
                if x4 + k < self.ctx_cols(plane) {
                    above |= self.above_level[plane][x4 + k] | self.above_dc[plane][x4 + k];
                }
            }
            for k in 0..h4 {
                if y4 + k < self.ctx_rows(plane) {
                    left |= self.left_level[plane][y4 + k] | self.left_dc[plane][y4 + k];
                }
            }
            let mut ctx = usize::from(above != 0) + usize::from(left != 0) + 7;
            // bw*bh > tw*th ⇒ the block is larger than the transform (4×4 tx in an ≥8×8 lossless block,
            // or a rectangular block whose chroma is split). Under TX_MODE_LARGEST tx == block, so this
            // never adds for the square lossy path.
            if block_w * block_h > tx_w * tx_h {
                ctx += 3;
            }
            ctx
        }
    }

    /// `dc_sign` context (§8.3.2): the sum of DC-sign contributions (`+1` positive, `-1` negative)
    /// over every MI cell the transform spans (`n4 = Tx_Width / 4` above cells and `n4` left cells).
    /// A single cell each is correct only for `TX_4X4`; an 8×8 transform with non-uniform neighbour
    /// DC categories needs the full sum or the arithmetic decode diverges.
    fn dc_sign_ctx(&self, plane: usize, x4: usize, y4: usize, w4: usize, h4: usize) -> usize {
        let mut s = 0i32;
        for k in 0..w4 {
            if x4 + k < self.ctx_cols(plane) {
                match self.above_dc[plane][x4 + k] {
                    1 => s -= 1,
                    2 => s += 1,
                    _ => {}
                }
            }
        }
        for k in 0..h4 {
            if y4 + k < self.ctx_rows(plane) {
                match self.left_dc[plane][y4 + k] {
                    1 => s -= 1,
                    2 => s += 1,
                    _ => {}
                }
            }
        }
        if s < 0 {
            1
        } else if s > 0 {
            2
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::tests::{enc420, grey4x4, planes_420};
    #[test]
    fn eob_pt_class_names_each_capped_coefficient_count() {
        // One transform per coded count 16..=1024, then the 64-sample sizes, whose coded region
        // caps each axis at 32 (§7.13) and so shares the 32-capped size's class.
        for (tx, class) in [
            (TxSize::Tx4x4, 0),
            (TxSize::Tx4x8, 1),
            (TxSize::Tx8x8, 2),
            (TxSize::Tx8x16, 3),
            (TxSize::Tx16x16, 4),
            (TxSize::Tx16x32, 5),
            (TxSize::Tx32x32, 6),
            (TxSize::Tx16x64, 5),
            (TxSize::Tx64x16, 5),
            (TxSize::Tx32x64, 6),
            (TxSize::Tx64x64, 6),
        ] {
            assert_eq!(eob_pt_class(tx), class, "{tx:?}");
        }
    }

    #[test]
    fn coeff_cdf_prices_each_eob_pt_class_against_the_cdf_it_codes() {
        // `probs` (the rate a trial pays) and `slot` (what the stream codes) spell the `eob_pt_*`
        // dispatch twice. Each class's table has its own alphabet size (5 symbols for 16
        // coefficients up to 11 for 1024), so a class priced against the `_` fallback's table is a
        // different slice.
        let mut cdfs = cdf::CdfContext::new(0);
        for class in 0..=6 {
            for ptype in 0..2 {
                let c = CoeffCdf::EobPt(class, ptype);
                let priced = c.probs(&cdfs).to_vec();
                assert_eq!(priced.len(), class + 5, "class {class}");
                assert_eq!(
                    priced,
                    c.slot(&mut cdfs).probs(),
                    "class {class}, ptype {ptype}"
                );
            }
        }
    }

    #[test]
    fn coeff_rate_is_what_coding_the_coefficients_tallies() {
        // `coeff_rate` and `code_coeffs` walk the same syntax; the first must price exactly what
        // the second spends, and leave the same neighbour contexts behind.
        let p = grey4x4();
        for (plane, tx_sym, levels) in [
            (0, 1, vec![0; 16]),
            (
                0,
                0,
                (0..16).map(|i| [3, -1, 0, 20][i % 4]).collect::<Vec<i32>>(),
            ),
            (1, 1, (0..16).map(|i| 40 - 3 * i).collect()),
        ] {
            let mut e = FrameEncoder::new(&p, 48);
            let (rate, cul, dc) =
                e.coeff_rate(plane, 0, 0, 4, 4, TxSize::Tx4x4, &levels, tx_sym, 0);
            e.sym.tally = Some(0);
            e.code_coeffs(plane, 0, 0, 4, 4, TxSize::Tx4x4, &levels, tx_sym, 0);
            assert_eq!(e.sym.tally, Some(rate), "plane {plane}: {levels:?}");
            assert_eq!((e.above_level[plane][0], e.above_dc[plane][0]), (cul, dc));
        }
    }

    #[test]
    fn entropy_context_grids_are_the_plane_mi_grid() {
        // §8.3.2 `maxX4`/`maxY4`: the luma MI grid scaled by the plane's subsampling. A `<<` in
        // place of the `>>` would give four times the columns rather than half.
        let p = planes_420(17, 13);
        let e = enc420(&p);
        assert_eq!((e.mi_cols, e.mi_rows), (6, 4));
        assert_eq!((e.ctx_cols(0), e.ctx_rows(0)), (6, 4));
        assert_eq!((e.ctx_cols(1), e.ctx_rows(1)), (3, 2));
        assert_eq!((e.ctx_cols(2), e.ctx_rows(2)), (3, 2));
        // Each context array is allocated at its own plane's grid, so the bound and the allocation
        // agree — indexing one with the other's extent would be out of bounds.
        for plane in 0..3 {
            assert_eq!(e.above_level[plane].len(), e.ctx_cols(plane));
            assert_eq!(e.left_level[plane].len(), e.ctx_rows(plane));
        }
    }

    #[test]
    fn context_writes_and_reads_stop_at_the_plane_grid() {
        // The `x4 + k < cols` guards are load-bearing for a run that overhangs the plane's context
        // grid: without them this indexes past the array. The residual loop does not currently
        // produce such a run, so the guards are exercised directly.
        let p = planes_420(17, 13);
        let mut e = enc420(&p);
        // Chroma grid is 3x2; write a 4-cell run starting at column 1, so cells 1 and 2 land and
        // cells 3 and 4 are past the edge.
        e.set_ctx(1, 1, 1, 4, 4, 9, 1);
        assert_eq!(e.above_level[1], vec![0, 9, 9]);
        assert_eq!(e.left_level[1], vec![0, 9]);
        assert_eq!(e.above_dc[1], vec![0, 1, 1]);
        // Reading back over the same overhanging run must also stay in bounds, and must see the
        // values that were written.
        let ctx = e.txb_skip_ctx(1, 1, 1, 8, 8, 16, 16);
        assert!(ctx >= 7, "chroma contexts start at 7, got {ctx}");

        // The luma ladder reads a *run* of cells, so distinct levels and a window starting away
        // from zero make `x4 + k` and `x4 * k` select different cells.
        e.above_level[0] = vec![0, 1, 9, 40, 2, 63];
        e.left_level[0] = vec![0, 0, 5, 3];
        // Two 4x4 cells from x4 = 1 see levels 1 and 9 (top = 9); rows from y4 = 1 see 0 and 5
        // (left = 5). Both non-zero and both ≤ 3 is false, so the ladder lands on 6.
        assert_eq!(e.txb_skip_ctx(0, 1, 1, 16, 16, 8, 8), 6);
        // A window whose *guard* arithmetic decides which cells are read: four cells from x4 = 2
        // are all inside the 6-column grid, so the highest level (63, at index 5) must be seen —
        // a guard computing `x4 * k` would stop early (2 * 3 = 6) and miss it. Every other cell in
        // the window is at most 3, so missing it moves `top` to 3 and the ladder to rung 5, rather
        // than leaving a second level above 3 to stand in for it. Four rows from y4 = 1 reach
        // exactly the 4-row edge, so the row guard must stop *before* index 4; `<=` reads one past
        // the end of the array. top = 63 and left = 5, both non-zero and both above 3 ⇒ rung 6.
        e.above_level[0] = vec![0, 1, 2, 3, 1, 63];
        assert_eq!(e.txb_skip_ctx(0, 2, 1, 32, 32, 16, 16), 6);
        // The column guard's own edge: four cells from x4 = 3 overhang the 6-column grid by one,
        // so the guard must stop before index 6 — `<=` reads past the array. Cells 3..=5 give
        // top = 63, rows 0..=3 give left = 5 ⇒ rung 6.
        assert_eq!(e.txb_skip_ctx(0, 3, 0, 32, 32, 16, 16), 6);
        let dc = e.dc_sign_ctx(1, 1, 1, 4, 4);
        assert_eq!(dc, 1, "two -1 categories sum negative");
    }

    #[test]
    fn chroma_txb_skip_context_adds_three_when_the_block_exceeds_the_transform() {
        // §8.3.2: `ctx = (above != 0) + (left != 0) + 7`, plus 3 when `bw * bh > w * h`. The `+ 7`
        // and the `+ 3` are separate terms, and the comparison is between the *plane residual* and
        // the transform — feeding it the luma block size would add 3 where it should not.
        let p = planes_420(17, 13);
        let e = enc420(&p);
        // Nothing written yet, so both neighbours are zero: ctx = 0 + 0 + 7.
        assert_eq!(e.txb_skip_ctx(1, 0, 0, 8, 8, 8, 8), 7);
        // Block larger than the transform ⇒ +3.
        assert_eq!(e.txb_skip_ctx(1, 0, 0, 8, 8, 4, 4), 10);
        // Luma uses a different ladder entirely, and returns 0 when block == transform.
        assert_eq!(e.txb_skip_ctx(0, 0, 0, 8, 8, 8, 8), 0);
        // …and only then: a transform matching the block in width alone takes the ladder, whose
        // first rung (both neighbours zero) is 1.
        assert_eq!(e.txb_skip_ctx(0, 0, 0, 8, 16, 8, 8), 1);
    }
}
