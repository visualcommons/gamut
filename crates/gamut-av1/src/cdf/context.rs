//! The adapting CDF state (AV1 §8.2.6, `disable_cdf_update = 0`): [`CdfContext`] is a tile's copy
//! of the [`super::defaults`] tables, nudged toward each symbol coded through [`encode`].

use gamut_bitstream::SymbolEncoder;

use super::defaults::*;

/// Number of `txSzCtx` classes (TX_4X4 … TX_64X64) the coefficient CDFs are keyed by (§8.3.2).
const TX_SZ_CTX: usize = 5;
/// `coeff_br` is keyed by `Min(txSzCtx, 3)`, so TX_64X64 shares the TX_32X32 row.
const BR_TX_SZ_CTX: usize = TX_SZ_CTX - 1;

/// One adapting CDF: the `N` cumulative values plus the spec's trailing `cdf[N]` element, which
/// counts how many times the symbol has been coded (saturating at 32) and sets the adaptation rate.
///
/// The [`super::defaults`] tables drop that trailing element, so a freshly initialised context
/// starts every counter at 0 — exactly the value the spec's `Default_*_Cdf` arrays carry.
#[derive(Clone, Copy, Debug)]
pub struct Cdf<const N: usize> {
    /// Cumulative probabilities rising to 32768 (`cdf[N - 1] == 32768`).
    cdf: [u16; N],
    /// The spec's `cdf[N]` adaptation counter.
    count: u16,
}

impl<const N: usize> Cdf<N> {
    /// Wraps a default table row, with the adaptation counter at its initial 0.
    const fn new(cdf: [u16; N]) -> Self {
        Self { cdf, count: 0 }
    }

    /// Borrows the CDF and its counter as the pair
    /// [`gamut_bitstream::SymbolEncoder::encode_symbol_adapt`] takes.
    pub fn slot(&mut self) -> CdfSlot<'_> {
        CdfSlot {
            cdf: &mut self.cdf,
            count: &mut self.count,
        }
    }

    /// The current (already adapted) probabilities, for deriving a temporary CDF such as
    /// `split_or_horz` (§8.2.6), which reads the live partition CDF but never updates it.
    pub fn probs(&self) -> &[u16] {
        &self.cdf
    }
}

/// A borrowed adapting CDF: the cumulative values and the counter that travels with them.
///
/// Returned by [`Cdf::slot`] so the accessors below can hand back rows of different alphabet sizes
/// (a partition CDF is 4 symbols at `bsl == 1` and 10 above it) from one function.
pub struct CdfSlot<'a> {
    /// The cumulative values.
    cdf: &'a mut [u16],
    /// The `cdf[N]` adaptation counter.
    count: &'a mut u16,
}

impl CdfSlot<'_> {
    /// The current (already adapted) probabilities, for pricing a symbol without coding it.
    pub fn probs(&self) -> &[u16] {
        self.cdf
    }
}

/// Codes `symbol` against `slot`, then applies the §8.2.6 adaptation in place.
///
/// This is the adapting counterpart of [`gamut_bitstream::SymbolEncoder::encode_symbol`]; a
/// decoder performs the identical update after parsing the symbol, so the two stay in lockstep.
pub fn encode(sym: &mut SymbolEncoder, symbol: usize, slot: CdfSlot<'_>) {
    sym.encode_symbol_adapt(symbol, slot.cdf, slot.count);
}

/// `Tx_Size_Sqr[txSz]` as an index into [`CdfContext::intra_tx_type_set2`]: 0 for TX_4X4, 1 for
/// TX_8X8, 2 for TX_16X16. `intra_tx_type` is coded only when `txSzSqrUp ≤ TX_16X16`, so larger
/// squares never reach this.
pub fn tx_sqr_class(tx_size_sqr: usize) -> usize {
    match tx_size_sqr {
        4 => 0,
        8 => 1,
        _ => 2,
    }
}

/// Wraps each row of a default table in a [`Cdf`].
fn rows<const N: usize, const M: usize>(src: &[[u16; N]; M]) -> [Cdf<N>; M] {
    std::array::from_fn(|i| Cdf::new(src[i]))
}

/// Wraps each row of a two-dimensional default table in a [`Cdf`].
fn rows2<const N: usize, const M: usize, const K: usize>(
    src: &[[[u16; N]; M]; K],
) -> [[Cdf<N>; M]; K] {
    std::array::from_fn(|i| rows(&src[i]))
}

/// The adapting CDFs a tile codes against (`TileXxxCdf` in the spec).
///
/// [`CdfContext::new`] performs the spec's `init_non_coeff_cdfs()` and `init_coeff_cdfs()`
/// (§5.9.2, invoked because `primary_ref_frame == PRIMARY_REF_NONE`): every field starts from the
/// §9.4 default in [`super::defaults`], and the coefficient families have their quantizer-context
/// dimension resolved once, as `init_coeff_cdfs` does. Each tile decodes independently, so a tile
/// starts from a fresh context rather than continuing the previous tile's.
///
/// Only the syntax elements this encoder emits are present. The constant tables beside it (scans,
/// context offsets, intra weights) are not probabilities and stay `static`.
#[derive(Clone, Debug)]
pub struct CdfContext {
    /// `TilePartitionW8Cdf` — 4 partition types at `bsl == 1`.
    pub partition_w8: [Cdf<4>; 4],
    /// `TilePartitionW16Cdf`.
    pub partition_w16: [Cdf<10>; 4],
    /// `TilePartitionW32Cdf`.
    pub partition_w32: [Cdf<10>; 4],
    /// `TilePartitionW64Cdf`.
    pub partition_w64: [Cdf<10>; 4],
    /// `TileSkipCdf`.
    pub skip: [Cdf<2>; 3],
    /// `TileUseWienerCdf`.
    pub restore_wiener: Cdf<2>,
    /// `TileSegmentIdCdf`.
    pub segment_id: [Cdf<8>; 3],
    /// `TileDeltaQCdf`.
    pub delta_q: Cdf<4>,
    /// `TileDeltaLFCdf` — same defaults as `delta_q`, but a distinct context that adapts apart.
    pub delta_lf: Cdf<4>,
    /// `TileIntraFrameYModeCdf`, indexed by the above/left mode contexts.
    pub intra_frame_y_mode: [[Cdf<13>; 5]; 5],
    /// `TileAngleDeltaCdf`, indexed by `YMode - V_PRED`.
    pub angle_delta: [Cdf<7>; 8],
    /// `TileUVModeCflAllowedCdf`, indexed by the luma mode.
    pub uv_mode_cfl_allowed: [Cdf<14>; 13],
    /// `TileUVModeCflNotAllowedCdf`, indexed by the luma mode.
    pub uv_mode_cfl_not_allowed: [Cdf<13>; 13],
    /// `TileCflSignCdf`.
    pub cfl_sign: Cdf<8>,
    /// `TileCflAlphaCdf`, indexed by the sign-derived context.
    pub cfl_alpha: [Cdf<16>; 6],
    /// `TilePaletteYModeCdf`, indexed by block-size and neighbour contexts.
    pub palette_y_mode: [[Cdf<2>; 3]; 7],
    /// `TilePaletteUVModeCdf`.
    pub palette_uv_mode: [Cdf<2>; 2],
    /// `TilePaletteYSizeCdf`, indexed by the block-size context.
    pub palette_y_size: [Cdf<7>; 7],
    /// `TilePaletteSize2YColorCdf`.
    pub palette_size_2_y_color: [Cdf<2>; 5],
    /// `TilePaletteSize3YColorCdf`.
    pub palette_size_3_y_color: [Cdf<3>; 5],
    /// `TilePaletteSize4YColorCdf`.
    pub palette_size_4_y_color: [Cdf<4>; 5],
    /// `TilePaletteSize5YColorCdf`.
    pub palette_size_5_y_color: [Cdf<5>; 5],
    /// `TilePaletteSize6YColorCdf`.
    pub palette_size_6_y_color: [Cdf<6>; 5],
    /// `TilePaletteSize7YColorCdf`.
    pub palette_size_7_y_color: [Cdf<7>; 5],
    /// `TilePaletteSize8YColorCdf`.
    pub palette_size_8_y_color: [Cdf<8>; 5],
    /// `TileFilterIntraCdf` for a 4×4 block.
    pub filter_intra_4x4: Cdf<2>,
    /// `TileFilterIntraCdf` for an 8×8 block.
    pub filter_intra_8x8: Cdf<2>,
    /// `TileFilterIntraCdf` for a 16×16 block.
    pub filter_intra_16x16: Cdf<2>,
    /// `TileFilterIntraCdf` for a 32×32 block.
    pub filter_intra_32x32: Cdf<2>,
    /// `TileFilterIntraModeCdf`.
    pub filter_intra_mode: Cdf<5>,
    /// `TileTxSizeCdf` for an 8×8 block (one depth bit).
    pub tx_size_8x8: [Cdf<2>; 3],
    /// `TileTxSizeCdf` for a 16×16 block.
    pub tx_size_16x16: [Cdf<3>; 3],
    /// `TileTxSizeCdf` for a 32×32 block.
    pub tx_size_32x32: [Cdf<3>; 3],
    /// `TileTxSizeCdf` for a 64×64 block.
    pub tx_size_64x64: [Cdf<3>; 3],
    /// `TileIntraTxTypeSet2Cdf[Tx_Size_Sqr[txSz]][intraDir]`, indexed by
    /// [`tx_sqr_class`] then the intra direction.
    ///
    /// The 4×4 and 8×8 *defaults* are the same uniform row for every direction, but the spec keeps
    /// one adapting context per (size, direction) pair — so they must not be collapsed here.
    pub intra_tx_type_set2: [[Cdf<5>; 13]; 3],
    /// `TileTxbSkipCdf`, indexed by `[txSzCtx][ctx]`.
    pub txb_skip: [[Cdf<2>; 13]; TX_SZ_CTX],
    /// `TileEobExtraCdf`, indexed by `[txSzCtx][ptype][eobPt - 3]`.
    pub eob_extra: [[[Cdf<2>; 9]; 2]; TX_SZ_CTX],
    /// `TileCoeffBaseEobCdf`, indexed by `[txSzCtx][ptype][ctx]`.
    pub coeff_base_eob: [[[Cdf<3>; 4]; 2]; TX_SZ_CTX],
    /// `TileCoeffBaseCdf`, indexed by `[txSzCtx][ptype][ctx]`.
    pub coeff_base: [[[Cdf<4>; 42]; 2]; TX_SZ_CTX],
    /// `TileCoeffBrCdf`, indexed by `[Min(txSzCtx, 3)][ptype][ctx]`.
    pub coeff_br: [[[Cdf<4>; 21]; 2]; BR_TX_SZ_CTX],
    /// `TileEobPt16Cdf`, indexed by plane type.
    pub eob_pt_16: [Cdf<5>; 2],
    /// `EobPt32Cdf` — the 32-coefficient transforms, reachable only with subsampled chroma.
    pub eob_pt_32: [Cdf<6>; 2],
    /// `TileEobPt64Cdf`, indexed by plane type.
    pub eob_pt_64: [Cdf<7>; 2],
    /// `TileEobPt128Cdf`, indexed by plane type.
    pub eob_pt_128: [Cdf<8>; 2],
    /// `TileEobPt256Cdf`, indexed by plane type.
    pub eob_pt_256: [Cdf<9>; 2],
    /// `TileEobPt512Cdf`, indexed by plane type.
    pub eob_pt_512: [Cdf<10>; 2],
    /// `TileEobPt1024Cdf`, indexed by plane type.
    pub eob_pt_1024: [Cdf<11>; 2],
    /// `TileDcSignCdf`, indexed by `[ptype][ctx]`. It carries no quantizer-context dimension.
    pub dc_sign: [[Cdf<2>; 3]; 2],
}

impl CdfContext {
    /// Initialises every CDF from its §9.4 default, resolving the coefficient tables' quantizer
    /// context `qctx` (0…3, derived from `base_q_idx`) once — the `init_coeff_cdfs` step. `qctx` is
    /// frame-level, so this is not affected by the per-superblock `delta_q`.
    ///
    /// The `eob_pt_*` tables additionally drop their tx-class dimension: every transform this
    /// encoder codes is `TX_CLASS_2D`, which is index 0.
    pub fn new(qctx: usize) -> Self {
        Self {
            partition_w8: rows(&PARTITION_W8),
            partition_w16: rows(&PARTITION_W16),
            partition_w32: rows(&PARTITION_W32),
            partition_w64: rows(&PARTITION_W64),
            skip: rows(&SKIP),
            restore_wiener: Cdf::new(RESTORE_WIENER),
            segment_id: rows(&SEGMENT_ID),
            delta_q: Cdf::new(DELTA_Q),
            delta_lf: Cdf::new(DELTA_LF),
            intra_frame_y_mode: rows2(&INTRA_FRAME_Y_MODE),
            angle_delta: rows(&ANGLE_DELTA),
            uv_mode_cfl_allowed: rows(&UV_MODE_CFL_ALLOWED),
            uv_mode_cfl_not_allowed: rows(&UV_MODE_CFL_NOT_ALLOWED),
            cfl_sign: Cdf::new(CFL_SIGN),
            cfl_alpha: rows(&CFL_ALPHA),
            palette_y_mode: rows2(&PALETTE_Y_MODE),
            palette_uv_mode: rows(&PALETTE_UV_MODE),
            palette_y_size: rows(&PALETTE_Y_SIZE),
            palette_size_2_y_color: rows(&PALETTE_SIZE_2_Y_COLOR),
            palette_size_3_y_color: rows(&PALETTE_SIZE_3_Y_COLOR),
            palette_size_4_y_color: rows(&PALETTE_SIZE_4_Y_COLOR),
            palette_size_5_y_color: rows(&PALETTE_SIZE_5_Y_COLOR),
            palette_size_6_y_color: rows(&PALETTE_SIZE_6_Y_COLOR),
            palette_size_7_y_color: rows(&PALETTE_SIZE_7_Y_COLOR),
            palette_size_8_y_color: rows(&PALETTE_SIZE_8_Y_COLOR),
            filter_intra_4x4: Cdf::new(FILTER_INTRA_4X4),
            filter_intra_8x8: Cdf::new(FILTER_INTRA_8X8),
            filter_intra_16x16: Cdf::new(FILTER_INTRA_16X16),
            filter_intra_32x32: Cdf::new(FILTER_INTRA_32X32),
            filter_intra_mode: Cdf::new(FILTER_INTRA_MODE),
            tx_size_8x8: rows(&TX_SIZE_8X8),
            tx_size_16x16: rows(&TX_SIZE_16X16),
            tx_size_32x32: rows(&TX_SIZE_32X32),
            tx_size_64x64: rows(&TX_SIZE_64X64),
            intra_tx_type_set2: [
                std::array::from_fn(|_| Cdf::new(INTRA_TX_TYPE_SET2)),
                std::array::from_fn(|_| Cdf::new(INTRA_TX_TYPE_SET2)),
                rows(&INTRA_TX_TYPE_SET2_16X16),
            ],
            txb_skip: [
                rows(&TXB_SKIP[qctx]),
                rows(&TXB_SKIP_8X8[qctx]),
                rows(&TXB_SKIP_16X16[qctx]),
                rows(&TXB_SKIP_32X32[qctx]),
                rows(&TXB_SKIP_64X64[qctx]),
            ],
            eob_extra: [
                rows2(&EOB_EXTRA[qctx]),
                rows2(&EOB_EXTRA_8X8[qctx]),
                rows2(&EOB_EXTRA_16X16[qctx]),
                rows2(&EOB_EXTRA_32X32[qctx]),
                rows2(&EOB_EXTRA_64X64[qctx]),
            ],
            coeff_base_eob: [
                rows2(&COEFF_BASE_EOB[qctx]),
                rows2(&COEFF_BASE_EOB_8X8[qctx]),
                rows2(&COEFF_BASE_EOB_16X16[qctx]),
                rows2(&COEFF_BASE_EOB_32X32[qctx]),
                rows2(&COEFF_BASE_EOB_64X64[qctx]),
            ],
            coeff_base: [
                rows2(&COEFF_BASE[qctx]),
                rows2(&COEFF_BASE_8X8[qctx]),
                rows2(&COEFF_BASE_16X16[qctx]),
                rows2(&COEFF_BASE_32X32[qctx]),
                rows2(&COEFF_BASE_64X64[qctx]),
            ],
            coeff_br: [
                rows2(&COEFF_BR[qctx]),
                rows2(&COEFF_BR_8X8[qctx]),
                rows2(&COEFF_BR_16X16[qctx]),
                rows2(&COEFF_BR_32X32[qctx]),
            ],
            eob_pt_16: std::array::from_fn(|p| Cdf::new(EOB_PT_16[qctx][p][0])),
            eob_pt_32: std::array::from_fn(|p| Cdf::new(EOB_PT_32[qctx][p][0])),
            eob_pt_64: std::array::from_fn(|p| Cdf::new(EOB_PT_64[qctx][p][0])),
            eob_pt_128: std::array::from_fn(|p| Cdf::new(EOB_PT_128[qctx][p][0])),
            eob_pt_256: std::array::from_fn(|p| Cdf::new(EOB_PT_256[qctx][p][0])),
            eob_pt_512: rows(&EOB_PT_512[qctx]),
            eob_pt_1024: rows(&EOB_PT_1024[qctx]),
            dc_sign: rows2(&DC_SIGN),
        }
    }

    /// The `partition` CDF for `bsl` (`Mi_Width_Log2`) at neighbour context `ctx` (§9.4).
    pub fn partition(&mut self, bsl: usize, ctx: usize) -> CdfSlot<'_> {
        match bsl {
            1 => self.partition_w8[ctx].slot(),
            2 => self.partition_w16[ctx].slot(),
            3 => self.partition_w32[ctx].slot(),
            _ => self.partition_w64[ctx].slot(),
        }
    }

    /// The live `partition` probabilities, from which `split_or_horz`/`split_or_vert` derive their
    /// temporary CDF. `bsl` is never 1 there, so the 4-symbol row is unreachable.
    pub fn partition_probs(&self, bsl: usize, ctx: usize) -> &[u16] {
        match bsl {
            2 => self.partition_w16[ctx].probs(),
            3 => self.partition_w32[ctx].probs(),
            _ => self.partition_w64[ctx].probs(),
        }
    }

    /// The `palette_color_idx_y` CDF for a luma palette of size `n` (2..=8) at color context `ctx`.
    pub fn palette_color_y(&mut self, n: usize, ctx: usize) -> CdfSlot<'_> {
        match n {
            2 => self.palette_size_2_y_color[ctx].slot(),
            3 => self.palette_size_3_y_color[ctx].slot(),
            4 => self.palette_size_4_y_color[ctx].slot(),
            5 => self.palette_size_5_y_color[ctx].slot(),
            6 => self.palette_size_6_y_color[ctx].slot(),
            7 => self.palette_size_7_y_color[ctx].slot(),
            _ => self.palette_size_8_y_color[ctx].slot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a slot's cumulative values (the counter is not part of the identity being pinned).
    fn probs_of(slot: CdfSlot<'_>) -> Vec<u16> {
        slot.cdf.to_vec()
    }

    #[test]
    fn palette_color_accessor_selects_the_size_table() {
        // Each palette size 2..=8 maps to its own §9.4 color CDF; the recon oracle never coded a
        // 3-colour palette, so a deleted match arm (n falling through to the size-8 table) is
        // invisible to it. Pin the table per size across all color contexts.
        let mut c = CdfContext::new(0);
        for ctx in 0..5 {
            assert_eq!(
                probs_of(c.palette_color_y(2, ctx)),
                PALETTE_SIZE_2_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(3, ctx)),
                PALETTE_SIZE_3_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(4, ctx)),
                PALETTE_SIZE_4_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(5, ctx)),
                PALETTE_SIZE_5_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(6, ctx)),
                PALETTE_SIZE_6_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(7, ctx)),
                PALETTE_SIZE_7_Y_COLOR[ctx]
            );
            assert_eq!(
                probs_of(c.palette_color_y(8, ctx)),
                PALETTE_SIZE_8_Y_COLOR[ctx]
            );
        }
    }

    #[test]
    fn partition_accessors_select_the_bsl_table() {
        // `partition` and `partition_probs` must agree on the row for every `bsl` the encoder
        // reaches, or `split_or_horz` would derive its temporary CDF from a different context than
        // the one `partition` adapts.
        let mut c = CdfContext::new(0);
        for ctx in 0..4 {
            assert_eq!(probs_of(c.partition(1, ctx)), PARTITION_W8[ctx]);
            assert_eq!(probs_of(c.partition(2, ctx)), PARTITION_W16[ctx]);
            assert_eq!(probs_of(c.partition(3, ctx)), PARTITION_W32[ctx]);
            assert_eq!(probs_of(c.partition(4, ctx)), PARTITION_W64[ctx]);
            for bsl in 2..=4 {
                assert_eq!(
                    probs_of(c.partition(bsl, ctx)),
                    c.partition_probs(bsl, ctx),
                    "bsl {bsl} ctx {ctx}"
                );
            }
        }
    }

    #[test]
    fn coeff_tables_resolve_the_quantizer_context() {
        // `init_coeff_cdfs` picks one `qctx` slice per frame. A context built for `qctx` must hold
        // that slice — an off-by-one here would still round-trip (the decoder derives `qctx` from
        // `base_q_idx` itself, so it would desync instead, but only at qindex boundaries).
        for qctx in 0..4 {
            let c = CdfContext::new(qctx);
            assert_eq!(c.txb_skip[0][0].probs(), TXB_SKIP[qctx][0]);
            assert_eq!(c.txb_skip[4][0].probs(), TXB_SKIP_64X64[qctx][0]);
            assert_eq!(c.coeff_base[2][1][7].probs(), COEFF_BASE_16X16[qctx][1][7]);
            assert_eq!(c.coeff_br[3][0][5].probs(), COEFF_BR_32X32[qctx][0][5]);
            assert_eq!(c.eob_pt_16[1].probs(), EOB_PT_16[qctx][1][0]);
            assert_eq!(c.eob_pt_32[0].probs(), EOB_PT_32[qctx][0][0]);
            assert_eq!(c.eob_pt_32[1].probs(), EOB_PT_32[qctx][1][0]);
            assert_eq!(c.eob_pt_1024[0].probs(), EOB_PT_1024[qctx][0]);
        }
        // TX_64X64 shares the TX_32X32 `coeff_br` row (`Min(txSzCtx, 3)`), so the table has four
        // classes, not five.
        assert_eq!(CdfContext::new(0).coeff_br.len(), BR_TX_SZ_CTX);
    }

    #[test]
    fn intra_tx_type_keeps_one_context_per_size_and_direction() {
        // `TileIntraTxTypeSet2Cdf[Tx_Size_Sqr[txSz]][intraDir]`. The TX_4X4 and TX_8X8 defaults are
        // the same uniform row for every direction, which makes collapsing them free while CDFs are
        // static — and a decoder desync the moment they adapt. Pin that the contexts are distinct
        // storage, not aliases.
        let mut c = CdfContext::new(0);
        for row in &c.intra_tx_type_set2[..2] {
            for cdf in row {
                assert_eq!(cdf.probs(), INTRA_TX_TYPE_SET2);
            }
        }
        for (dir, want) in INTRA_TX_TYPE_SET2_16X16.iter().enumerate() {
            assert_eq!(c.intra_tx_type_set2[2][dir].probs(), want);
        }
        assert_eq!(tx_sqr_class(4), 0);
        assert_eq!(tx_sqr_class(8), 1);
        assert_eq!(tx_sqr_class(16), 2);

        // Adapting one (size, direction) pair must leave every other pair untouched.
        let mut sym = SymbolEncoder::new();
        encode(&mut sym, 3, c.intra_tx_type_set2[0][5].slot());
        assert_ne!(c.intra_tx_type_set2[0][5].probs(), INTRA_TX_TYPE_SET2);
        assert_eq!(c.intra_tx_type_set2[0][4].probs(), INTRA_TX_TYPE_SET2);
        assert_eq!(c.intra_tx_type_set2[1][5].probs(), INTRA_TX_TYPE_SET2);
    }

    #[test]
    fn a_fresh_context_starts_with_a_zero_adaptation_count() {
        // The defaults drop the spec's trailing `cdf[N]` element, whose initial value is 0.
        let c = CdfContext::new(0);

        assert_eq!(c.skip[0].count, 0);
    }

    #[test]
    fn coding_a_symbol_adapts_that_context() {
        let mut c = CdfContext::new(0);
        let before = c.skip[0].cdf;
        let mut sym = SymbolEncoder::new();

        encode(&mut sym, 0, c.skip[0].slot());

        assert_eq!(c.skip[0].count, 1);
        assert_ne!(c.skip[0].cdf, before, "coding a symbol must adapt its CDF");
    }

    #[test]
    fn contexts_adapt_independently_of_their_siblings() {
        // The claim that makes per-context adaptation worth having, and the one that fails on its
        // own: an implementation sharing one CDF across contexts would satisfy both tests above
        // and still be wrong, because coding in context 0 would drag context 1 with it.
        let mut c = CdfContext::new(0);
        let mut sym = SymbolEncoder::new();

        encode(&mut sym, 0, c.skip[0].slot());

        assert_eq!(c.skip[1].cdf, SKIP[1], "a sibling context was disturbed");
        assert_eq!(c.skip[1].count, 0);
    }
}
