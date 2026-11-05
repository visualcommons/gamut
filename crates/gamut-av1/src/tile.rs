//! The all-intra tile encoder: superblock/partition iteration (§5.11.2/.4), intra prediction
//! (§7.11.2), the forward transform (via `gamut-dsp`), and coefficient coding with full context
//! derivation (§5.11.39, §8.3.2). One or more uniform tiles (§5.11.1) are coded with this path.
//!
//! Two paths share this code, selected by `qindex` (`base_q_idx`):
//! - **Lossless** (`qindex == 0`): forced `TX_4X4` Walsh–Hadamard; prediction neighbours are the
//!   source samples (which equal the reconstruction under lossless); partitions are
//!   `PARTITION_NONE` except at the right/bottom frame edges.
//! - **Lossy** (`qindex > 0`): recursive `PARTITION_NONE`/`HORZ`/`VERT`/`SPLIT` down to 4×4, every
//!   block at the frame quantizer and loop-filter level — per-superblock `delta_q`/`delta_lf` and
//!   segmentation (`SEG_LVL_ALT_Q`) are coded only under the test-only [`BlockVariation`]. Each luma
//!   block codes a transform under `TX_MODE_SELECT` (square `tx_depth` 0..2; sizes `TX_4X4`..
//!   `TX_64X64`, with `TX_32X32`/`TX_64X64` DCT-only). The luma prediction mode is chosen per block
//!   from all 13 intra modes (`DC`, the eight directional `V`/`H`/`D45`/`D135`/`D113`/`D157`/`D203`/
//!   `D67` with `angle_delta`, and `SMOOTH`/`SMOOTH_V`/`SMOOTH_H`/`PAETH`, §7.11.2), plus recursive
//!   **filter-intra** (§7.11.2.3, signaled as `DC_PRED` + `use_filter_intra`) and **palette**
//!   (§7.11.4); the luma transform type is chosen from `TX_SET_INTRA_2`
//!   (`{IDTX, DCT_DCT, ADST_ADST, ADST_DCT, DCT_ADST}`) and signaled. Chroma is `DC_PRED` or
//!   **chroma-from-luma** (`UV_CFL_PRED`, §7.11.5, when it beats DC) + `DCT_DCT`. Prediction reads a
//!   **reconstruction buffer** that the encoder maintains exactly as the decoder would (predict →
//!   transform → quantize → dequantize → inverse-transform → add → store), so the encoder's
//!   reconstruction is bit-exact with a conformant decoder's output; the in-loop filters
//!   (deblocking, CDEF, loop restoration, superres) are applied afterward in `filter`, with the
//!   deblocking level and CDEF strengths chosen against the source ([`FrameEncoder::encode`]).
//!
//! Loop restoration is signalled per unit *inside* the tiles, at the first superblock of each
//! unit, but can only be decided once the whole frame is reconstructed and filtered. So a frame
//! is coded with no restoration first; when the encoder then finds a unit the Wiener filter
//! improves, the tiles are coded a second time with the restoration units signalled
//! ([`FrameEncoder::recoding`]). The second pass replays every partition and mode decision the
//! first one coded, superblock by superblock, rather than searching again: nothing it decides
//! reads the restoration symbols, so the replayed blocks reconstruct exactly as the first pass's
//! did. The record costs one small entry per coded block and per decided partition, held for the
//! whole frame; the second pass costs the coding without the search.
//!
//! The frame is coded on the MI-unit grid (`mi_cols*4 × mi_rows*4`, dimensions rounded up to a
//! multiple of 8); the out-of-frame padding is edge-replicated and cropped away on decode.
//!
//! How many of those candidates the lossy path actually tries is a [`Search`], resolved from the
//! caller's `Effort` preset; a bare `FrameEncoder` runs the full search. The partition (bottom-up, 8×8 to
//! 64×64), the luma mode (with filter-intra) jointly with the transform depth, the transform type
//! and the CfL alphas are each decided by rate–distortion cost:
//! every candidate is *trialled* — reconstructed exactly as it would be coded, its symbols priced
//! from the adapting CDFs instead of coded ([`coder::Coding::Trial`], [`coder::Coder`]'s tally) —
//! and the trial is undone, so the encoder state the chosen candidate is then coded from is the
//! one it was priced from.
//!
//! The frame-level driver — the [`FrameEncoder`] state, the tile and superblock loop, the
//! restoration second pass and the in-loop filter choice — lives here; the stages it drives are
//! submodules: `partition` (the partition tree and its search), `mode` (block mode info and the
//! mode decision), `intra` (prediction and palette), `recon` (residual coding and
//! reconstruction), `coeff` (coefficient syntax and contexts) and `coder` (the symbol coder and
//! rate units they all share).

use std::num::NonZeroUsize;
use std::sync::Arc;

use gamut_color::{ChromaSubsampling, Planar8, Planar16};

use self::coder::{Coder, encode_subexp_with_ref};
use self::mode::{ALL_ANGLE_DELTAS, ALL_CFL_MAGNITUDES, DIRECTIONAL_MODES, Decided};
use self::partition::DecidedPartition;
use self::recon::TX_TYPE_TRIALS;
use crate::cdf;
use crate::encoder::Effort;
use crate::filter::{CdefBlocks, CdefStrengths, FrameFilters, LrUnits};
use crate::geom::PlaneGeom;
use crate::headers::TileLayout;
use crate::quant::{ac_q, dc_q};

mod coder;
mod coeff;
mod intra;
mod mode;
mod partition;
mod recon;

pub(crate) use mode::SEG_ALT_Q;
pub(crate) use recon::{Reconstruction, chroma_tx_log2};

/// `DC_PRED` (§3): the only mode used for chroma and the lossless path.
const DC_PRED: u8 = 0;
/// `V_PRED` (§3): directional, `pAngle = 90` (copy the above row).
const V_PRED: u8 = 1;
/// `H_PRED` (§3): directional, `pAngle = 180` (copy the left column).
const H_PRED: u8 = 2;
/// `D45_PRED` (§3): directional, `pAngle = 45` (zone 1, needs above-right samples).
const D45_PRED: u8 = 3;
/// `D203_PRED` (§3): directional, `pAngle = 203` (zone 3, needs below-left samples).
const D203_PRED: u8 = 7;
/// `D67_PRED` (§3): directional, `pAngle = 67` (zone 1, needs above-right samples).
const D67_PRED: u8 = 8;
/// `D135_PRED` (§3): directional, `pAngle = 135`.
const D135_PRED: u8 = 4;
/// `D113_PRED` (§3): directional, `pAngle = 113`.
const D113_PRED: u8 = 5;
/// `D157_PRED` (§3): directional, `pAngle = 157`.
const D157_PRED: u8 = 6;
/// `SMOOTH_PRED` (§3).
const SMOOTH_PRED: u8 = 9;
/// `SMOOTH_V_PRED` (§3).
const SMOOTH_V_PRED: u8 = 10;
/// `SMOOTH_H_PRED` (§3).
const SMOOTH_H_PRED: u8 = 11;
/// `PAETH_PRED` (§3).
const PAETH_PRED: u8 = 12;
/// `UV_CFL_PRED` (§3): the chroma-from-luma `uv_mode` value (the 14th symbol of the CfL-allowed
/// `uv_mode` CDF). Selected for chroma on the lossy path when CfL beats plain `DC_PRED`.
const UV_CFL_PRED: u8 = 13;
/// The luma intra modes the lossy path searches over. All are bit-exact from only the immediate
/// above/left neighbours plus the top-left corner: the non-directional modes by construction, and
/// the included directional modes because `angle_delta = 0` (4×4 blocks) and `pAngle ∈ {90, 180}`
/// (cardinal) or `90 < pAngle < 180` (zone 2) keep every reference index within `[-1, 3]`. The
/// directional angles `D45`/`D67` (zone 1) and `D203` (zone 3) additionally read the above-right /
/// below-left reconstruction via the `BlockDecoded` availability map.
const LUMA_MODES: [u8; 13] = [
    DC_PRED,
    V_PRED,
    H_PRED,
    D45_PRED,
    D135_PRED,
    D113_PRED,
    D157_PRED,
    D203_PRED,
    D67_PRED,
    SMOOTH_PRED,
    SMOOTH_V_PRED,
    SMOOTH_H_PRED,
    PAETH_PRED,
];

/// The mode-decision search a frame runs: a [`Effort`] preset resolved into per-tool candidate
/// sets. Every field narrows what the encoder *tries*; none changes how a chosen candidate is
/// signalled or reconstructed, so every value codes a conformant, bit-exact stream.
///
/// The limits are candidate lists rather than on/off flags wherever an off flag would only skip
/// work whose result is already discarded: an empty list does no work, and there is no guard left
/// whose inversion no output could detect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Search {
    /// The directional base modes a square ≥8×8 block tries. There is no 4×4 counterpart: the
    /// one preset that prunes directional modes (`Fastest`) also never codes a lossy 4×4 block —
    /// `split_from` 16 stops chosen 8×8 splits, and on the even `MiCols`/`MiRows` grid no 8×8
    /// block overhangs the frame, so none is forced — so `select_luma_mode` always tries every
    /// [`LUMA_MODES`] entry.
    directional: &'static [u8],
    /// The `AngleDeltaY` values each directional base mode is tried at.
    angle_deltas: &'static [i8],
    /// How many recursive filter-intra modes (`0..n`) a `DC_PRED` block tries.
    filter_intra_modes: u8,
    /// The CfL alpha magnitudes tried, or `None` to code chroma `DC_PRED` without searching.
    cfl_magnitudes: Option<&'static [i32]>,
    /// How many of [`TX_TYPE_TRIALS`] a square luma transform tries after `DCT_DCT`.
    tx_type_trials: usize,
    /// The square sizes the partition search offers `PARTITION_HORZ`/`PARTITION_VERT` at.
    rect_sizes: &'static [usize],
    /// The smallest square block side the partition search may *choose* to split: 8 lets it reach
    /// 4×4 leaves, 16 stops it at 8×8. A split the frame edge forces is taken whatever this says.
    /// Stored as the split threshold itself rather than as the leaf size, so the comparison that
    /// reads it has no arithmetic whose mutants would agree on every block size.
    split_from: usize,
    /// Whether `decide_palette` looks for a luma palette.
    palette: bool,
}

impl Search {
    /// The search a [`Effort`] preset runs. The table is documented on [`Effort`].
    pub(crate) fn for_effort(effort: Effort) -> Self {
        let thorough = Self {
            directional: &DIRECTIONAL_MODES,
            angle_deltas: &ALL_ANGLE_DELTAS,
            filter_intra_modes: 5,
            cfl_magnitudes: Some(&ALL_CFL_MAGNITUDES),
            tx_type_trials: TX_TYPE_TRIALS.len(),
            rect_sizes: &[16, 32],
            split_from: 8,
            palette: true,
        };
        let balanced = Self {
            angle_deltas: &[0],
            ..thorough
        };
        let fast = Self {
            filter_intra_modes: 0,
            cfl_magnitudes: Some(&[1, 2, 4, 8, 16]),
            tx_type_trials: 1,
            ..balanced
        };
        match effort {
            Effort::Thorough => thorough,
            Effort::Balanced => balanced,
            Effort::Fast => fast,
            Effort::Fastest => Self {
                directional: &[],
                cfl_magnitudes: None,
                tx_type_trials: 0,
                rect_sizes: &[],
                split_from: 16,
                palette: false,
                ..fast
            },
        }
    }
}

/// The encoder's borrowed source planes: 8-bit, or 10/12-bit.
///
/// One enum rather than a generic parameter, because the *only* place the width matters is
/// [`FrameEncoder::sample`] — every other step already works in `i32` or `u16`. Widening an 8-bit
/// carrier into `u16` up front would instead cost a full second copy of the image on the common
/// path, and monomorphizing the 3,000-line coding path over the sample type would cost it twice in
/// compiled code.
#[derive(Clone, Copy)]
pub(crate) enum PlaneSource<'a> {
    /// Planes of 8-bit samples, from a [`Planar8`].
    Eight([&'a [u8]; 3]),
    /// Planes of 10- or 12-bit samples, from a [`Planar16`].
    High([&'a [u16]; 3]),
}

impl PlaneSource<'_> {
    /// The sample at `index` within `plane`, widened to the `i32` every coding step works in.
    #[inline]
    fn at(self, plane: usize, index: usize) -> i32 {
        match self {
            PlaneSource::Eight(planes) => i32::from(planes[plane][index]),
            PlaneSource::High(planes) => i32::from(planes[plane][index]),
        }
    }
}

/// Whether a lossy frame varies its quantizer and loop-filter level from block to block.
///
/// The encoder has no content-driven reason to vary either yet, so a default encode codes every
/// block at the frame's `base_q_idx` and loop-filter level: `segmentation_enabled = 0` and
/// `delta_q_present = 0` (which also leaves `delta_lf_present` uncoded, §5.9.18). The variation the
/// lossy path used to apply unconditionally is kept behind [`Self::SyntaxExercise`] so the
/// segmentation, `delta_q` and `delta_lf` syntax stays under oracle test: there a block's segment
/// comes from its position (`(r + c) % 3`, alt-Q `+20`/`+50`, [`SEG_ALT_Q`]), each superblock signals
/// a `delta_q` cycling `+1/−1/+2` and a `delta_lf` of `0/1`, all by position alone. That is a
/// content-blind quality penalty on two thirds of the blocks (#1011), so nothing outside this
/// crate's tests selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum BlockVariation {
    /// Every block at the frame-level quantizer and loop-filter level.
    #[default]
    Frame,
    /// Position-only segment, `delta_q` and `delta_lf` assignment, to exercise their syntax.
    SyntaxExercise,
}

impl BlockVariation {
    /// Whether a frame at `qindex` codes the position-only variation: segmentation with
    /// [`SEG_ALT_Q`], and `delta_q`/`delta_lf` present. A lossless frame never does — §5.9.18 codes
    /// `delta_q_present` only when `base_q_idx > 0`, and a segment quantizer above 0 would make the
    /// frame lossy.
    pub(crate) fn exercised(self, qindex: u8) -> bool {
        self == Self::SyntaxExercise && qindex > 0
    }
}

/// The coefficient-CDF quantizer context (§8.3.2) for a quantizer index: `0` if `≤ 20`, `1` if
/// `≤ 60`, `2` if `≤ 120`, else `3`.
const fn qctx_for(qindex: i32) -> usize {
    if qindex <= 20 {
        0
    } else if qindex <= 60 {
        1
    } else if qindex <= 120 {
        2
    } else {
        3
    }
}

/// Number of loop-restoration units spanning `size` samples on a `unit`-sample grid (§5.11.57).
///
/// A trailing partial unit smaller than half a unit is absorbed into the previous one rather than
/// starting its own, hence the `+ unit / 2` before the divide; a frame smaller than one unit still
/// has one. Extracted from `write_lr` so the rounding is directly testable: gamut's unit grid is
/// 256 samples and every fixture is smaller than that, so in place this arithmetic is pinned at
/// exactly one point (`1`) and any mutation of it survives.
pub(crate) fn lr_unit_count(size: usize, unit: usize) -> usize {
    ((size + unit / 2) / unit).max(1)
}

/// The partition and mode decisions one superblock was coded with, in the order its blocks were
/// coded: what [`FrameEncoder::recoding`] replays instead of searching again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SbDecisions {
    partitions: Vec<DecidedPartition>,
    blocks: Vec<Decided>,
}

/// The all-intra frame encoder: codes each tile of a [`TileLayout`] and keeps the frame-wide
/// reconstruction and per-MI maps the in-loop filters read afterwards.
///
/// `Clone` is what the tile-parallel path is built on: a worker thread gets a copy of the encoder
/// before any tile is coded, codes its share of the tiles in it, and the tiles' rectangles are
/// merged back with [`FrameEncoder::absorb_tile`].
#[derive(Clone)]
pub(crate) struct FrameEncoder<'a> {
    planes: PlaneSource<'a>,
    /// AV1's `NumPlanes` (§5.5.2): 1 for a monochrome frame, 3 otherwise. Every chroma syntax
    /// element and every chroma coding step is gated on this — `HasChroma` (§5.11.5) is
    /// `NumPlanes > 1`, and since the coding path is 4:4:4 or monochrome, it is constant for the
    /// whole frame rather than varying per block.
    num_planes: usize,
    /// Per-plane sample geometry. `geom[0]` is luma; the MI grid, the partition search and every
    /// entropy context stay on the **luma** grid, so a site that indexes `mi_cols`/`mi_rows` reads
    /// `geom[0]` even when it is working on a chroma plane.
    geom: [PlaneGeom; 3],
    /// The frame's chroma subsampling, for the §5.11.5/§5.11.38 block-level derivations.
    subsampling: ChromaSubsampling,
    /// §5.11.35 `MaxLumaW`/`MaxLumaH`: the right/bottom edge of the last in-bounds **luma**
    /// transform block of the current `decode_block`, written on the plane-0 residual pass and read
    /// by CfL to clamp its luma reads. Plane 0 is fully coded before chroma, so these are current by
    /// the time CfL runs.
    max_luma_w: usize,
    max_luma_h: usize,
    mi_cols: usize,
    mi_rows: usize,
    /// MI boundaries of the tile currently being encoded (`0`/`mi_cols`/`mi_rows` for a single tile).
    /// A tile decodes independently, so neighbour availability (prediction samples and symbol
    /// contexts) stops at the tile edge even when it is not the frame edge: `tile_c0` bounds every
    /// left neighbour (`AvailL`) and `tile_r0` every above one (`AvailU`, §7.3.1 `is_inside`).
    tile_c0: usize,
    tile_x0: usize,
    tile_r0: usize,
    /// `tile_r0` in luma samples, the row counterpart of `tile_x0`.
    tile_y0: usize,
    /// Exclusive right/bottom MI boundary of the current tile (`MiColEnd`/`MiRowEnd`).
    tile_c1: usize,
    tile_r1: usize,
    /// `base_q_idx`; 0 ⇒ lossless WHT path, > 0 ⇒ lossy DCT path.
    qindex: u8,
    /// Coefficient-CDF quantizer context (§8.3.2): 0 if `base_q_idx ≤ 20`, 1 if ≤ 60, 2 if ≤ 120,
    /// else 3. Frame-level — `init_coeff_cdfs` derives it from `base_q_idx`, so it is **not** changed
    /// by the per-superblock `delta_q` (only the `dc_quant`/`ac_quant` step sizes follow that).
    qctx: usize,
    dc_quant: i32,
    ac_quant: i32,
    /// `CurrentQIndex` (§7.12.2): `base_q_idx` plus the accumulated per-superblock `delta_q`.
    current_qindex: i32,
    /// `CurrentDeltaLF` (§7.14.4): the accumulated per-superblock loop-filter-level delta.
    current_dlf: i32,
    /// Whether the frame codes the position-only block variation ([`BlockVariation::exercised`]):
    /// segmentation is enabled, so every block codes a segment id, and `delta_q_present` /
    /// `delta_lf_present` are set, so the first block of each superblock signals a `delta_q` and a
    /// `delta_lf`, gated by `read_deltas`. Off by default, when every block keeps the frame's
    /// quantizer and loop-filter level.
    block_variation: bool,
    read_deltas: bool,
    /// Bits per sample (8, 10, or 12); sets the reconstruction clamp range and dequant/transform
    /// bit depth.
    bit_depth: u32,
    /// Reconstructed samples per plane on the coded grid (lossy path only), each in
    /// `0..=(1 << bit_depth) - 1`.
    recon: [Vec<u16>; 3],
    /// The tile's range coder — or, while a decision trials a candidate, a rate tally in its place.
    sym: Coder,
    /// The adapting CDFs the current tile codes against (§8.2.6, `disable_cdf_update = 0`). Reset
    /// to the frame defaults at each tile boundary, since tiles decode independently.
    cdfs: cdf::CdfContext,
    above_level: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_level: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    /// `Mi_Width_Log2` of the block covering each MI cell (for the partition context).
    mi_bsl: Vec<u8>,
    /// `Mi_Height_Log2` of the block covering each MI cell (the height counterpart of `mi_bsl`), for
    /// the deblock filter to size a rectangular block's chroma horizontal edges.
    mi_bsl_h: Vec<u8>,
    /// `AbovePartitionContext`/`LeftPartitionContext` (§5.11.4): per-column / per-row partition
    /// context bitmasks updated from `AL_PART_CTX` after each terminal partition. A given block level
    /// reads bit `bsl - 1`; this distinguishes HORZ/VERT/NONE neighbours (which `mi_bsl` alone cannot).
    above_partition: Vec<u8>,
    left_partition: Vec<u8>,
    /// The luma intra mode (`YMode`) chosen for each MI cell, for the `intra_frame_y_mode` context.
    mi_ymode: Vec<u8>,
    /// The `skip` flag of the block covering each MI cell, for the `skip` context (§8.3.2).
    mi_skip: Vec<u8>,
    /// `SegmentIds` of the block covering each MI cell (§5.11.9), for the `segment_id` spatial
    /// prediction and per-segment quantizer.
    mi_segid: Vec<u8>,
    /// `PaletteSizeY` of the block covering each MI cell (0 = no palette), for the `has_palette_y`
    /// context and `get_palette_cache`.
    mi_psize: Vec<u8>,
    /// `PaletteColors` (the sorted luma palette, up to 8 entries) of the block covering each MI cell,
    /// for `get_palette_cache` (only the first `mi_psize` entries are meaningful).
    mi_pcolors: Vec<[u16; 8]>,
    /// `DeltaLF` (loop-filter-level delta) of the block covering each MI cell (§7.14.4), consumed by
    /// the deblocking filter to vary the level per superblock.
    mi_dlf: Vec<i8>,
    /// `Tx_Width_Log2` of the transform covering each MI cell (`2` for 4×4, `3` for 8×8), consumed by
    /// the deblocking loop filter to locate transform edges and pick the filter size.
    tx_log2: Vec<u8>,
    /// `Tx_Height_Log2` of the transform covering each MI cell — equals `tx_log2` for square
    /// transforms, differs for rectangular ones. The `tx_depth` context reads a left neighbour's tx
    /// *height* and an above neighbour's tx *width*, so both are tracked.
    tx_log2_h: Vec<u8>,
    /// Top-left MI position of the superblock currently being encoded (for `BlockDecoded` indexing).
    sb_r: usize,
    sb_c: usize,
    /// `BlockDecoded` (§5.11.3) for the current superblock's luma plane: one flag per 4×4, indexed
    /// `[(by + 1) * BD_STRIDE + (bx + 1)]` for superblock-relative `(by, bx)` in `-1..=16`. Drives
    /// the above-right / below-left reference-sample availability for directional prediction.
    block_decoded: Vec<u8>,
    /// The candidate sets the lossy mode decisions search (full by default).
    search: Search,
    /// The mode decisions blocks reached while they were trialled by the partition search
    /// ([`FrameEncoder::choose_partition`]); a block coded again — by an enclosing trial, or for
    /// real — is coded as decided instead of searched again. Emptied at each superblock: an entry
    /// a losing alternative left is never looked up again, since no other alternative codes a
    /// block at the same position and size.
    decided: Vec<Decided>,
    /// The partitions the search chose for the regions it trialled, reused the same way.
    decided_partitions: Vec<DecidedPartition>,
    /// The decisions the current superblock has been coded with so far (not trialled).
    sb_decisions: SbDecisions,
    /// Every coded superblock's [`SbDecisions`], raster order over the superblock grid.
    record: Vec<SbDecisions>,
    /// A first pass's [`Self::record`], replayed instead of searched ([`Self::recoding`]).
    replay: Option<Arc<Vec<SbDecisions>>>,
    /// The deblocking level and CDEF strengths to apply, or `None` to choose them against the
    /// source after the tiles are coded.
    filters: Option<FrameFilters>,
    /// The luma restoration units to signal, or `None` for `FrameRestorationType[0] =
    /// RESTORE_NONE`, which codes none.
    lr: Option<LrUnits>,
    /// `SuperresDenom` (§5.9.8), or `SUPERRES_NUM` (8) without superres: the scale §5.11.57 maps
    /// a superblock's columns onto the upscaled restoration-unit grid by.
    lr_denom: usize,
}

/// One tile's MI rectangle: `MiRowStart..MiRowEnd` × `MiColStart..MiColEnd` (§5.11.1), all on
/// superblock boundaries except where the frame itself ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TileRect {
    r0: usize,
    r1: usize,
    c0: usize,
    c1: usize,
}

/// `SUPERRES_NUM` (§3): the numerator of every superres scale, so `SuperresDenom / SUPERRES_NUM`
/// is the upscale factor and a denominator of `SUPERRES_NUM` is no upscale.
const SUPERRES_NUM: usize = 8;

/// Row stride of [`FrameEncoder::block_decoded`]: a 64×64 superblock is 16 MI wide, plus the `-1`
/// guard row/column and the `+16` below/right edge ⇒ indices `-1..=16` → 18 entries.
const BD_STRIDE: usize = 18;

impl<'a> FrameEncoder<'a> {
    /// Whether chroma is coded (`HasChroma`, §5.11.5 — `NumPlanes > 1`).
    fn has_chroma(&self) -> bool {
        self.num_planes > 1
    }

    /// `1 << (BitDepth - 1)` — the mid-grey AV1 predicts with when a block has no neighbour at all
    /// (§7.11.2). The `AboveRow` and `LeftCol` defaults are this minus and plus one respectively,
    /// which is what makes `PAETH_PRED` resolve to the corner in that case.
    fn neutral(&self) -> i32 {
        1 << (self.bit_depth - 1)
    }

    /// Creates an encoder over 8-bit identity planes (Y=G, U=B, V=R) at quantizer `qindex`
    /// (`base_q_idx`; 0 selects the lossless path). 4:4:4, or monochrome with a luma plane only.
    pub(crate) fn new(planes: &'a Planar8, qindex: u8) -> Self {
        Self::over(
            PlaneSource::Eight(planes.planes()),
            planes.width() as usize,
            planes.height() as usize,
            planes.subsampling(),
            8,
            qindex,
        )
    }

    /// Creates an encoder over high-bit-depth planes, at the depth the buffer itself carries.
    pub(crate) fn new16(planes: &'a Planar16, qindex: u8) -> Self {
        Self::over(
            PlaneSource::High(planes.planes()),
            planes.width() as usize,
            planes.height() as usize,
            planes.subsampling(),
            u32::from(planes.bit_depth().bits()),
            qindex,
        )
    }

    /// The shared constructor: everything below the sample width is identical, so the two entry
    /// points differ only in the carrier they borrow and the depth they report.
    fn over(
        planes: PlaneSource<'a>,
        width: usize,
        height: usize,
        subsampling: ChromaSubsampling,
        bit_depth: u32,
        qindex: u8,
    ) -> Self {
        let mi_cols = 2 * ((width + 7) >> 3);
        let mi_rows = 2 * ((height + 7) >> 3);
        let geom = PlaneGeom::frame(width, height, mi_cols, mi_rows, subsampling);
        let recon = if qindex > 0 {
            std::array::from_fn(|p| vec![0u16; geom[p].len()])
        } else {
            [Vec::new(), Vec::new(), Vec::new()]
        };
        Self {
            planes,
            num_planes: subsampling.num_planes(),
            geom,
            subsampling,
            // Before any luma transform block is coded the whole coded plane is the bound, which is
            // both a sound starting value and the widest the per-block updates can ever set.
            max_luma_w: geom[0].coded_w,
            max_luma_h: geom[0].coded_h,
            mi_cols,
            mi_rows,
            tile_c0: 0,
            tile_x0: 0,
            tile_r0: 0,
            tile_y0: 0,
            tile_c1: mi_cols,
            tile_r1: mi_rows,
            qindex,
            qctx: qctx_for(i32::from(qindex)),
            dc_quant: dc_q(bit_depth, i32::from(qindex)),
            ac_quant: ac_q(bit_depth, i32::from(qindex)),
            current_qindex: i32::from(qindex),
            current_dlf: 0,
            block_variation: false,
            read_deltas: false,
            bit_depth,
            recon,
            sym: Coder::default(),
            cdfs: cdf::CdfContext::new(qctx_for(i32::from(qindex))),
            // Sized per plane (§8.3.2 `maxX4`/`maxY4`), so a chroma context array is half the
            // luma one under subsampling rather than over-allocated and wrongly indexed.
            above_level: std::array::from_fn(|p| vec![0; mi_cols >> geom[p].ss_x]),
            above_dc: std::array::from_fn(|p| vec![0; mi_cols >> geom[p].ss_x]),
            left_level: std::array::from_fn(|p| vec![0; mi_rows >> geom[p].ss_y]),
            left_dc: std::array::from_fn(|p| vec![0; mi_rows >> geom[p].ss_y]),
            mi_bsl: vec![0; mi_cols * mi_rows],
            mi_bsl_h: vec![0; mi_cols * mi_rows],
            above_partition: vec![0; mi_cols],
            left_partition: vec![0; mi_rows],
            mi_ymode: vec![0; mi_cols * mi_rows],
            mi_skip: vec![0; mi_cols * mi_rows],
            mi_segid: vec![0; mi_cols * mi_rows],
            mi_psize: vec![0; mi_cols * mi_rows],
            mi_pcolors: vec![[0u16; 8]; mi_cols * mi_rows],
            mi_dlf: vec![0; mi_cols * mi_rows],
            tx_log2: vec![2; mi_cols * mi_rows],
            tx_log2_h: vec![2; mi_cols * mi_rows],
            sb_r: 0,
            sb_c: 0,
            block_decoded: vec![0; BD_STRIDE * BD_STRIDE],
            // The full search, not `Effort::default()`: every public entry point replaces this
            // with its caller's preset through `with_search`, so only the crate's own tests
            // construct an encoder that keeps it, and they are written against the full search.
            search: Search::for_effort(Effort::Thorough),
            decided: Vec::new(),
            decided_partitions: Vec::new(),
            sb_decisions: SbDecisions::default(),
            record: vec![SbDecisions::default(); mi_cols.div_ceil(16) * mi_rows.div_ceil(16)],
            replay: None,
            filters: None,
            lr: None,
            lr_denom: SUPERRES_NUM,
        }
    }

    /// Replaces the mode-decision search (the full one by default).
    pub(crate) fn with_search(mut self, search: Search) -> Self {
        self.search = search;
        self
    }

    /// Replaces the block variation ([`BlockVariation::Frame`] by default). The frame header must
    /// be written with the same `variation`, which is what tells the decoder to read the symbols
    /// it adds.
    pub(crate) fn with_block_variation(mut self, variation: BlockVariation) -> Self {
        self.block_variation = variation.exercised(self.qindex);
        self
    }

    /// Makes this encoder the second pass of a frame `first` coded: the tiles are coded again
    /// with the luma restoration units `lr` signalled (`superres_denom` is `SuperresDenom`, or
    /// `None` without superres), every superblock coded with the decisions `first` recorded for
    /// it rather than searched, and the in-loop filters `first` chose applied unchanged.
    ///
    /// Nothing the first pass decided reads a restoration symbol — `use_wiener` has a CDF of its
    /// own and the Wiener taps are coded as raw bits — so this pass's reconstruction is the first
    /// pass's, and only the stream differs.
    pub(crate) fn recoding(
        mut self,
        first: &Reconstruction,
        lr: LrUnits,
        superres_denom: Option<usize>,
    ) -> Self {
        self.replay = Some(Arc::clone(&first.decisions));
        self.filters = Some(first.filters);
        self.lr = Some(lr);
        self.lr_denom = superres_denom.unwrap_or(SUPERRES_NUM);
        self
    }

    /// The index of the superblock at MI `(r, c)` in [`Self::record`].
    fn sb_index(&self, r: usize, c: usize) -> usize {
        (r / 16) * self.mi_cols.div_ceil(16) + c / 16
    }

    /// `clear_block_decoded_flags` (§5.11.3) for the luma plane of the superblock at MI `(r, c)`:
    /// the row above and column left of the superblock (within the frame) are available; the
    /// interior is not yet decoded; the below-left corner is forced unavailable.
    fn clear_block_decoded(&mut self, r: usize, c: usize) {
        const SB4: isize = 16;
        let sb_width4 = (self.tile_c1 - c) as isize;
        let sb_height4 = (self.tile_r1 - r) as isize;
        for y in -1..=SB4 {
            for x in -1..=SB4 {
                // The row above the superblock and the column to its left (within the frame) are
                // already reconstructed; the interior is not yet decoded.
                let avail = (y < 0 && x < sb_width4) || (x < 0 && y < sb_height4);
                self.block_decoded[(y + 1) as usize * BD_STRIDE + (x + 1) as usize] = avail.into();
            }
        }
        self.block_decoded[(SB4 + 1) as usize * BD_STRIDE] = 0; // [sbSize4][-1] = 0
    }

    /// Reads a `BlockDecoded` flag at superblock-relative `(by, bx)` (both in `-1..=16`).
    fn block_decoded_at(&self, by: isize, bx: isize) -> bool {
        self.block_decoded[(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] != 0
    }

    /// Marks decoded in `BlockDecoded` (§5.11.34) the `h4`×`w4` luma 4×4s whose top-left is at
    /// MI `(r, c)`. Both callers mark cells of the current superblock — a luma transform lies in
    /// its block, a block in its 64×64 superblock — so each cell is in `0..16` of it and needs
    /// no bounds guard: such a guard holds for every block, and its `&&` → `||` is equivalent
    /// (#910, #922). The index is `usize`, so a cell before the superblock underflows and
    /// panics. The palette branch of `encode_block` and `code_plane` share it.
    fn mark_block_decoded(&mut self, r: usize, c: usize, h4: usize, w4: usize) {
        for by in r - self.sb_r..r - self.sb_r + h4 {
            let row = (by + 1) * BD_STRIDE + 1 + c - self.sb_c;
            self.block_decoded[row..row + w4].fill(1);
        }
    }

    /// Signals the loop-restoration units (§5.11.57 `read_lr`) whose top-left falls in the superblock
    /// at MI `(r, c)`, as [`Self::lr`] decided them: `use_wiener`, and for a restored unit the
    /// default reference filter — every coded coefficient delta is zero. Chroma is `RESTORE_NONE`,
    /// and with no [`Self::lr`] so is luma, and nothing is coded. The unit grid is 256×256
    /// (`lr_unit_shift = 2`) on the upscaled frame; the filter is applied by
    /// [`crate::filter::loop_restore_wiener_luma`].
    fn write_lr(&mut self, r: usize, c: usize) {
        const UNIT: usize = crate::filter::LR_UNIT;
        // A 64×64 superblock: `Num_4x4_Blocks_Wide/High[sbSize]`.
        const SB4: usize = 16;
        // (offset, num_syms, k) for the three Wiener half-taps (§5.11.58 read_lr_unit).
        const TAPS: [(i32, i32, u32); 3] = [(5, 16, 1), (23, 32, 2), (17, 64, 3)];
        let Some(lr) = &self.lr else {
            return;
        };
        // Luma geometry deliberately: loop restoration signals `RESTORE_NONE` for both chroma
        // planes, so the unit grid is defined on luma alone (§5.11.57). The columns are counted
        // over `UpscaledWidth`, so with superres a superblock's MI columns are scaled up by
        // `SuperresDenom / SUPERRES_NUM` onto it.
        let row_start = (r * 4).div_ceil(UNIT);
        let row_end = lr.rows.min(((r + SB4) * 4).div_ceil(UNIT));
        let (numerator, denominator) = (4 * self.lr_denom, UNIT * SUPERRES_NUM);
        let col_start = (c * numerator).div_ceil(denominator);
        let col_end = lr.cols.min(((c + SB4) * numerator).div_ceil(denominator));
        let units: Vec<bool> = (row_start..row_end)
            .flat_map(|row| (col_start..col_end).map(move |col| row * lr.cols + col))
            .map(|unit| lr.wiener[unit])
            .collect();
        for wiener in units {
            // use_wiener, then (if set) the vertical then horizontal half-taps.
            self.sym
                .encode(usize::from(wiener), self.cdfs.restore_wiener.slot());
            if wiener {
                for _pass in 0..2 {
                    for (j, &(off, num_syms, k)) in TAPS.iter().enumerate() {
                        let v = crate::filter::WIENER_DEFAULT[j] + off;
                        // Straight into the range coder: loop restoration is signalled per
                        // superblock, outside any block decision, so it is never trialled.
                        encode_subexp_with_ref(&mut self.sym.range, v, num_syms, k, v);
                    }
                }
            }
        }
    }

    /// Codes `tile` (`decode_tile`, §5.11.2) and returns its symbol-coded bytes.
    ///
    /// Everything `decode_tile` resets is reset here, so the bytes are a function of the tile's
    /// rectangle and the source alone — whatever this encoder coded before. A fresh range coder;
    /// the CDFs back to the frame defaults (`init_non_coeff_cdfs`/`init_coeff_cdfs`, each tile
    /// adapting from its own copy); `CurrentQIndex` and `DeltaLF` back to the frame values;
    /// `clear_above_context`; and, per superblock row, `clear_left_context`. Neighbour availability
    /// stops at the tile's edges through `tile_c0`/`tile_r0`.
    fn encode_tile(&mut self, tile: TileRect) -> Vec<u8> {
        const SB4: usize = 16; // 64×64 superblock in MI units
        self.tile_c0 = tile.c0;
        self.tile_x0 = tile.c0 * 4;
        self.tile_c1 = tile.c1;
        self.tile_r0 = tile.r0;
        self.tile_y0 = tile.r0 * 4;
        self.tile_r1 = tile.r1;
        self.cdfs = cdf::CdfContext::new(self.qctx);
        self.set_quant(i32::from(self.qindex));
        self.current_dlf = 0;
        for plane in 0..self.num_planes {
            self.above_level[plane].fill(0);
            self.above_dc[plane].fill(0);
        }
        // Stepping by whole superblocks over an exclusive range, rather than a hand-rolled
        // `while r < r1 { ... r += SB4 }`: the comparison in that form is mutable to `<=` without
        // any test noticing, because the extra iteration lands entirely outside the frame and
        // `encode_partition` emits nothing for it.
        for r in (tile.r0..tile.r1).step_by(SB4) {
            for plane in 0..self.num_planes {
                self.left_level[plane].fill(0);
                self.left_dc[plane].fill(0);
            }
            for c in (tile.c0..tile.c1).step_by(SB4) {
                self.sb_r = r;
                self.sb_c = c;
                // ReadDeltas is armed per superblock (§7.18) under the block-variation exercise;
                // the first block then signals delta_q and delta_lf.
                self.read_deltas = self.block_variation;
                self.clear_block_decoded(r, c);
                let sb = self.sb_index(r, c);
                // A second pass starts each superblock from what the first one coded there, so
                // every decision is found as already reached rather than searched.
                let replayed = self.replay.as_ref().map(|replay| replay[sb].clone());
                let replayed = replayed.unwrap_or_default();
                self.decided = replayed.blocks;
                self.decided_partitions = replayed.partitions;
                // A lossless frame never has `Self::lr` set (only the lossy path in `encoder.rs`
                // re-codes with restoration units), so `write_lr` codes nothing for it; a
                // `qindex > 0` guard here would be a second copy of that fact, and an equivalent
                // `>` → `>=` mutant.
                self.write_lr(r, c);
                self.encode_partition(r, c, 64);
                self.record[sb] = std::mem::take(&mut self.sb_decisions);
            }
        }
        std::mem::take(&mut self.sym.range).finish()
    }

    /// Copies `tile`'s rectangle of what the in-loop filters read after the tiles are coded — the
    /// reconstruction, the per-MI maps `filter::deblock` and `filter::cdef` take, and the tile's
    /// superblocks' recorded decisions — from `from`, an encoder that coded that tile, into
    /// `self`.
    ///
    /// Tiles never read outside their own rectangle, so the merged state is what coding the tiles
    /// one after another in `self` would have left, as far as anything after the tiles can see.
    /// The other per-MI maps (`mi_ymode`, `mi_segid`, `mi_psize`, `mi_pcolors`) are contexts read
    /// only while coding the tile that wrote them, so they are deliberately not merged.
    fn absorb_tile(&mut self, from: &Self, tile: TileRect) {
        let cols = self.mi_cols;
        for r in tile.r0..tile.r1 {
            let row = r * cols + tile.c0..r * cols + tile.c1;
            for (dst, src) in [
                (&mut self.mi_bsl, &from.mi_bsl),
                (&mut self.mi_bsl_h, &from.mi_bsl_h),
                (&mut self.mi_skip, &from.mi_skip),
                (&mut self.tx_log2, &from.tx_log2),
                (&mut self.tx_log2_h, &from.tx_log2_h),
            ] {
                dst[row.clone()].copy_from_slice(&src[row.clone()]);
            }
            self.mi_dlf[row.clone()].copy_from_slice(&from.mi_dlf[row]);
        }
        for r in (tile.r0..tile.r1).step_by(16) {
            for c in (tile.c0..tile.c1).step_by(16) {
                let sb = self.sb_index(r, c);
                self.record[sb].clone_from(&from.record[sb]);
            }
        }
        // The reconstruction exists on the lossy path only (it *is* the source when lossless), and
        // a monochrome frame's chroma planes are empty; both cases copy nothing.
        for (plane, g) in self.geom.iter().enumerate() {
            if self.recon[plane].is_empty() {
                continue;
            }
            let (x0, x1) = ((tile.c0 * 4) >> g.ss_x, (tile.c1 * 4) >> g.ss_x);
            for y in (tile.r0 * 4) >> g.ss_y..(tile.r1 * 4) >> g.ss_y {
                let row = y * g.coded_w + x0..y * g.coded_w + x1;
                self.recon[plane][row.clone()].copy_from_slice(&from.recon[plane][row]);
            }
        }
    }

    /// Codes every tile in `tiles` (raster order) and returns their bytes in that order, on up to
    /// `threads` threads. The bytes and the merged state do not depend on `threads`.
    ///
    /// Worker `w` of `min(threads, tiles)` codes tiles `w, w + workers, …`: worker 0 on this thread
    /// in `self`, every other one on a scoped thread in a clone of `self` taken before any tile is
    /// coded, whose rectangles are then merged back. One worker is therefore exactly the sequential
    /// encode, with no clone and no thread. A worker that cannot be spawned has its share coded
    /// here instead, so running out of threads costs time, never the encode. A panic on a worker is
    /// a bug, and is re-raised here rather than turned into an error.
    ///
    /// `wasm32` has no threads to spawn, so one worker codes every tile there whatever `threads`
    /// says.
    ///
    /// Memory: each spawned worker holds its own copy of the frame-wide state, chiefly the
    /// reconstruction (three `u16` planes on the coded grid on the lossy path) — so peak memory
    /// grows by about that much per extra thread.
    fn encode_tiles(&mut self, tiles: &[TileRect], threads: NonZeroUsize) -> Vec<Vec<u8>> {
        let workers = if cfg!(target_arch = "wasm32") {
            1
        } else {
            threads.get().min(tiles.len())
        };
        let share = |w: usize| tiles.iter().copied().enumerate().skip(w).step_by(workers);
        let mut bytes = vec![Vec::new(); tiles.len()];
        std::thread::scope(|scope| {
            let spawned: Vec<_> = (1..workers)
                .map(|w| {
                    let mut enc = self.clone();
                    let handle = std::thread::Builder::new().spawn_scoped(scope, move || {
                        let coded: Vec<(usize, Vec<u8>)> =
                            share(w).map(|(i, t)| (i, enc.encode_tile(t))).collect();
                        (enc, coded)
                    });
                    (w, handle)
                })
                .collect();
            for (i, t) in share(0) {
                bytes[i] = self.encode_tile(t);
            }
            for (w, handle) in spawned {
                let Ok(handle) = handle else {
                    for (i, t) in share(w) {
                        bytes[i] = self.encode_tile(t);
                    }
                    continue;
                };
                let (enc, coded) = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                for (i, b) in coded {
                    self.absorb_tile(&enc, tiles[i]);
                    bytes[i] = b;
                }
            }
        });
        bytes
    }

    /// Codes the frame in `layout`'s tiles on up to `threads` threads, then runs the in-loop filters
    /// over the merged reconstruction. Returns each tile's bytes (raster order) and the
    /// reconstruction (lossy path).
    pub(crate) fn encode(
        mut self,
        layout: &TileLayout,
        threads: NonZeroUsize,
    ) -> (Vec<Vec<u8>>, Reconstruction) {
        let tiles: Vec<TileRect> = layout
            .row_starts
            .windows(2)
            .flat_map(|rows| {
                layout.col_starts.windows(2).map(move |cols| TileRect {
                    r0: rows[0],
                    r1: rows[1],
                    c0: cols[0],
                    c1: cols[1],
                })
            })
            .collect();
        let tile_bytes = self.encode_tiles(&tiles, threads);
        // Deblocking loop filter (§7.14): a post-process on the final reconstruction (intra
        // prediction during encoding read the pre-filter samples, so this does not affect any
        // prediction). The lossless path is `CodedLossless`, where the filter is disabled.
        let mut planes = std::mem::take(&mut self.recon);
        let mut deblocked_luma = Vec::new();
        let mut filters = FrameFilters::default();
        if self.qindex > 0 {
            filters = self.filters.unwrap_or_else(|| self.choose_filters(&planes));
            self.deblock(&mut planes, filters.deblock_level);
            // CDEF reads the deblocked reconstruction and produces a deringed one (§7.15). The
            // deblocked luma is retained for the loop-restoration stripe boundaries (§7.17); loop
            // restoration itself is applied by the caller after the optional superres upscale.
            deblocked_luma = planes[0].clone();
            planes = self.cdef(&planes, &self.cdef_blocks(&planes), filters.cdef);
        }
        let recon = Reconstruction {
            planes,
            geom: self.geom,
            bit_depth: self.bit_depth,
            deblocked_luma,
            filters,
            decisions: Arc::new(self.record),
        };
        (tile_bytes, recon)
    }

    /// Chooses the deblocking level and then the CDEF strengths by the distortion each leaves
    /// against the source, on the coded tiles' unfiltered reconstruction `planes`, as
    /// [`crate::filter::choose_deblock_level`] and [`crate::filter::choose_cdef`] search them.
    ///
    /// Cost: this is called once per frame, from the frame-level path after `encode_tiles` has
    /// returned, over the whole reconstruction. It is the same search at every `Effort`, so a
    /// faster preset does not shorten it, and it runs on one thread whatever the caller's thread
    /// cap — only the tile coding before it is concurrent. Measured, it is about 6–8% of the
    /// tile-coding time: 0.65 s against 11.0 s at 256×256, and 2.68 s against 33.0 s at 512×512.
    /// It is deliberately neither pruned nor threaded here (issue #798 records only its cost).
    fn choose_filters(&self, planes: &[Vec<u16>; 3]) -> FrameFilters {
        let deblock_level = crate::filter::choose_deblock_level(|level| {
            let mut deblocked = planes.clone();
            self.deblock(&mut deblocked, level);
            self.frame_sse(&deblocked, 0..self.num_planes)
        });
        let mut deblocked = planes.clone();
        self.deblock(&mut deblocked, deblock_level);
        let blocks = self.cdef_blocks(&deblocked);
        let cdef = crate::filter::choose_cdef(|pri, sec| {
            let strengths = CdefStrengths {
                y_pri: pri,
                y_sec: sec,
                uv_pri: pri,
                uv_sec: sec,
            };
            let out = self.cdef(&deblocked, &blocks, strengths);
            (
                self.frame_sse(&out, 0..1),
                self.frame_sse(&out, 1..self.num_planes),
            )
        });
        FrameFilters {
            deblock_level,
            cdef,
        }
    }

    /// Deblocks `planes` in place at frame `level`, over this frame's transform and block maps.
    fn deblock(&self, planes: &mut [Vec<u16>; 3], level: u8) {
        crate::filter::deblock(
            planes,
            &self.geom,
            self.subsampling,
            self.mi_cols,
            &self.tx_log2,
            &self.tx_log2_h,
            &self.mi_bsl,
            &self.mi_bsl_h,
            &self.mi_dlf,
            level,
            self.bit_depth,
        );
    }

    /// The CDEF direction search over the deblocked `planes`.
    fn cdef_blocks(&self, planes: &[Vec<u16>; 3]) -> CdefBlocks {
        CdefBlocks::analyse(
            &planes[0],
            &self.geom,
            &self.mi_skip,
            self.mi_cols,
            self.bit_depth,
        )
    }

    /// `planes` filtered by CDEF at `strengths`.
    fn cdef(
        &self,
        planes: &[Vec<u16>; 3],
        blocks: &CdefBlocks,
        strengths: CdefStrengths,
    ) -> [Vec<u16>; 3] {
        crate::filter::cdef(
            planes,
            &self.geom,
            blocks,
            strengths,
            self.num_planes,
            self.bit_depth,
        )
    }

    /// The squared error against the source of the visible samples of `planes` in `range`, each
    /// plane on its coded stride.
    fn frame_sse(&self, planes: &[Vec<u16>; 3], range: std::ops::Range<usize>) -> u64 {
        let mut sse = 0u64;
        for plane in range {
            let g = self.geom[plane];
            for y in 0..g.h {
                for x in 0..g.w {
                    let d = i64::from(planes[plane][y * g.coded_w + x])
                        - i64::from(self.planes.at(plane, y * g.w + x));
                    sse += (d * d) as u64;
                }
            }
        }
        sse
    }

    /// Padded (edge-replicated) source sample of `plane` at coded-grid position `(x, y)`.
    fn sample(&self, plane: usize, x: usize, y: usize) -> i32 {
        let g = self.geom[plane];
        let xx = x.min(g.w - 1);
        let yy = y.min(g.h - 1);
        self.planes.at(plane, yy * g.w + xx)
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::Planar8;
    use gamut_core::Dimensions;

    use super::*;
    use crate::geom::plane_residual_size;

    /// Non-zero test dimensions.
    pub(super) fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions::new(w, h).unwrap()
    }

    #[test]
    fn every_emitted_block_shape_has_a_codable_chroma_residual() {
        // §6.10.4: `get_plane_residual_size(subSize, 1)` must never be BLOCK_INVALID. Enumerates
        // the shapes `encode_partition` can produce — squares 4..64 and the PARTITION_HORZ/VERT
        // halves at 16 and 32 — against each layout the encoder codes.
        //
        // 4:2:2 is the constraint that bites: it rules out every taller-than-wide shape, which is
        // why `partitions_offered` must not offer PARTITION_VERT there.
        let squares = [(4, 4), (8, 8), (16, 16), (32, 32), (64, 64)];
        let horz = [(16, 8), (32, 16)];
        let vert = [(8, 16), (16, 32)];

        for ss in [ChromaSubsampling::Cs444, ChromaSubsampling::Cs420] {
            for (bw, bh) in squares.iter().chain(&horz).chain(&vert) {
                assert!(
                    plane_residual_size(*bw, *bh, 1, ss).is_some(),
                    "{bw}x{bh} must be codable at {ss:?}"
                );
            }
        }
        for (bw, bh) in squares.iter().chain(&horz) {
            assert!(
                plane_residual_size(*bw, *bh, 1, ChromaSubsampling::Cs422).is_some(),
                "{bw}x{bh} must be codable at 4:2:2"
            );
        }
        for (bw, bh) in vert {
            assert!(
                plane_residual_size(bw, bh, 1, ChromaSubsampling::Cs422).is_none(),
                "{bw}x{bh} must be rejected at 4:2:2"
            );
        }
    }

    /// An `n x n` frame in `ss` whose luma is `luma(x, y)` and whose chroma planes are flat.
    pub(super) fn flat_chroma_luma(
        n: usize,
        ss: ChromaSubsampling,
        luma: impl Fn(usize, usize) -> u8,
    ) -> Planar8 {
        let y = (0..n)
            .flat_map(|row| (0..n).map(move |col| (col, row)))
            .map(|(col, row)| luma(col, row))
            .collect();
        let (cw, ch) = ss.chroma_dimensions(n as u32, n as u32);
        let chroma = vec![128u8; (cw * ch) as usize];
        Planar8::from_planes_subsampled(dims(n as u32, n as u32), ss, [y, chroma.clone(), chroma])
            .expect("valid planes")
    }

    /// `planes`' encoder at `qindex`, running the search `effort` resolves to.
    pub(super) fn at_effort(planes: &Planar8, qindex: u8, effort: Effort) -> FrameEncoder<'_> {
        FrameEncoder::new(planes, qindex).with_search(Search::for_effort(effort))
    }

    /// A 64x64 identity frame of high-contrast texture: every 8x8 is textured enough to split.
    pub(super) fn busy64() -> Planar8 {
        let mut rgb = vec![0u8; 64 * 64 * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let (x, y) = (i % 64, i / 64);
            px.copy_from_slice(&[((x * 37) ^ (y * 91)) as u8, (x * 13 + y * 7) as u8, 128]);
        }
        Planar8::from_rgb8_identity(&rgb, dims(64, 64)).unwrap()
    }

    #[test]
    fn clear_block_decoded_ends_the_above_row_and_left_column_at_the_tile_edge() {
        // §5.11.3: `BlockDecoded[-1][x]` is set for `x < sbWidth4` and `BlockDecoded[y][-1]` for
        // `y < sbHeight4`, both counted to the tile's end, so a superblock at a tile's right or
        // bottom edge sees no above-right or below-left samples in the next tile. A tile edge
        // inside the 16-MI superblock puts both bounds where a fixture can read either side; a
        // superblock away from the origin makes the bounds differences, not the tile's ends.
        let p = busy64();
        let mut e = FrameEncoder::new(&p, 40);
        (e.tile_r1, e.tile_c1) = (16 + 5, 16 + 7);
        e.clear_block_decoded(16, 16);
        assert!(e.block_decoded_at(-1, 6), "the tile's last column is above");
        assert!(
            !e.block_decoded_at(-1, 7),
            "the column past the tile is not"
        );
        assert!(e.block_decoded_at(4, -1), "the tile's last row is left");
        assert!(!e.block_decoded_at(5, -1), "the row past the tile is not");
    }

    #[test]
    fn clear_block_decoded_forces_only_the_below_left_corner_unavailable() {
        // §5.11.3 then clears `BlockDecoded[sbSize4][-1]`: in a tile that runs on past the
        // superblock, the left column is available down to row 15 and the corner below it is not.
        // Every other edge cell, the top-left corner among them, keeps its availability.
        let p = busy64();
        let mut e = FrameEncoder::new(&p, 40);
        (e.tile_r1, e.tile_c1) = (64, 64);
        e.clear_block_decoded(0, 0);
        assert!(e.block_decoded_at(-1, -1), "the top-left corner");
        assert!(e.block_decoded_at(-1, 16), "the above-right corner");
        assert!(e.block_decoded_at(15, -1), "the left column's last row");
        assert!(!e.block_decoded_at(16, -1), "the below-left corner");
    }

    #[test]
    fn mark_block_decoded_marks_exactly_its_cells_relative_to_the_superblock() {
        // A superblock away from the origin makes the MI position differ from the cell, and a
        // 2×3 run that starts off the superblock's corner makes rows, columns and each extent
        // distinguishable: the whole map is compared, so a mark anywhere else fails too.
        let p = busy64();
        let mut e = FrameEncoder::new(&p, 40);
        (e.sb_r, e.sb_c) = (16, 16);
        e.mark_block_decoded(16 + 4, 16 + 2, 2, 3);
        let marked: Vec<(isize, isize)> = (-1..=16)
            .flat_map(|by| (-1..=16).map(move |bx| (by, bx)))
            .filter(|&(by, bx)| e.block_decoded_at(by, bx))
            .collect();
        assert_eq!(marked, [(4, 2), (4, 3), (4, 4), (5, 2), (5, 3), (5, 4)]);
    }

    #[test]
    fn only_the_lossy_path_allocates_a_reconstruction() {
        // Lossless prediction reads the source itself (`dc_pred`'s `sample`), so the encoder
        // keeps no frame-sized reconstruction there; `absorb_tile` relies on the empty planes.
        let p = grey4x4();
        assert!(FrameEncoder::new(&p, 0).recon.iter().all(Vec::is_empty));
        let lossy = FrameEncoder::new(&p, 1);
        assert!(
            lossy
                .recon
                .iter()
                .all(|plane| plane.len() == lossy.geom[0].len())
        );
    }

    /// A 16x16 identity frame whose luma is `luma(x, y)` and whose chroma is flat mid-grey.
    pub(super) fn luma16(luma: impl Fn(usize, usize) -> u8) -> Planar8 {
        flat_chroma_luma(16, ChromaSubsampling::Cs444, luma)
    }

    /// `planes`' encoder at `qindex` under the full search, at the start of its first superblock,
    /// with every sample reconstructed as the source — as a decoder would have the neighbours at
    /// a high enough quality.
    pub(super) fn primed(planes: &Planar8, qindex: u8) -> FrameEncoder<'_> {
        let mut e = at_effort(planes, qindex, Effort::Thorough);
        e.clear_block_decoded(0, 0);
        for (plane, g) in e.geom.iter().enumerate() {
            for y in 0..g.coded_h {
                for x in 0..g.coded_w {
                    e.recon[plane][y * g.coded_w + x] = e.sample(plane, x, y) as u16;
                }
            }
        }
        e
    }

    #[test]
    fn block_variation_is_exercised_only_when_asked_for_on_a_lossy_frame() {
        // Example: the variation is coded only under the syntax exercise, never by default (#1011),
        // and never on a lossless frame, whose header cannot code `delta_q_present`.
        assert!(!BlockVariation::default().exercised(40));
        assert!(!BlockVariation::Frame.exercised(40));
        assert!(BlockVariation::SyntaxExercise.exercised(1));
        assert!(!BlockVariation::SyntaxExercise.exercised(0));
        let p = busy64();
        assert!(!FrameEncoder::new(&p, 40).block_variation);
        let exercised =
            FrameEncoder::new(&p, 40).with_block_variation(BlockVariation::SyntaxExercise);
        assert!(exercised.block_variation);
        let lossless =
            FrameEncoder::new(&p, 0).with_block_variation(BlockVariation::SyntaxExercise);
        assert!(!lossless.block_variation);
    }

    /// Everything a trial must leave as it found it, comparable.
    #[derive(PartialEq)]
    pub(super) struct Snapshot {
        recon: [Vec<u16>; 3],
        level: [[Vec<u8>; 3]; 4],
        maps: Vec<Vec<u8>>,
        pcolors: Vec<[u16; 8]>,
        dlf: Vec<i8>,
        partition: [Vec<u8>; 2],
        block_decoded: Vec<u8>,
        max_luma: (usize, usize),
        quant: (i32, i32, i32, i32, bool),
        tally: Option<u32>,
        stream: Vec<u8>,
        cdfs: String,
    }

    impl Snapshot {
        pub(super) fn of(e: &FrameEncoder<'_>) -> Self {
            Self {
                recon: e.recon.clone(),
                level: [
                    e.above_level.clone(),
                    e.above_dc.clone(),
                    e.left_level.clone(),
                    e.left_dc.clone(),
                ],
                maps: e.mi_maps().iter().map(|m| m.to_vec()).collect(),
                pcolors: e.mi_pcolors.clone(),
                dlf: e.mi_dlf.clone(),
                partition: [e.above_partition.clone(), e.left_partition.clone()],
                block_decoded: e.block_decoded.clone(),
                max_luma: (e.max_luma_w, e.max_luma_h),
                quant: (
                    e.current_qindex,
                    e.current_dlf,
                    e.dc_quant,
                    e.ac_quant,
                    e.read_deltas,
                ),
                tally: e.sym.tally,
                stream: e.sym.range.clone().finish(),
                cdfs: format!("{:?}", e.cdfs),
            }
        }
    }

    /// A 4:2:0 `Planar8` whose planes are filled by `f(plane, x, y)` at each plane's own size.
    pub(super) fn planes_420_with(
        w: u32,
        h: u32,
        f: impl Fn(usize, usize, usize) -> u8,
    ) -> Planar8 {
        let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
        let luma = (0..h as usize)
            .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
            .map(|(x, y)| f(0, x, y))
            .collect();
        let chroma = |p: usize| -> Vec<u8> {
            (0..ch)
                .flat_map(|y| (0..cw).map(move |x| (x, y)))
                .map(|(x, y)| f(p, x, y))
                .collect()
        };
        Planar8::from_planes_subsampled(
            dims(w, h),
            ChromaSubsampling::Cs420,
            [luma, chroma(1), chroma(2)],
        )
        .unwrap()
    }

    /// A 4:2:0 encoder over a 17x13 frame: coded luma 24x16, coded chroma 12x8, MI grid 6x4.
    pub(super) fn enc420(planes: &Planar8) -> FrameEncoder<'_> {
        FrameEncoder::new(planes, 40)
    }

    pub(super) fn planes_420(w: u32, h: u32) -> Planar8 {
        let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
        Planar8::from_planes_subsampled(
            dims(w, h),
            ChromaSubsampling::Cs420,
            [
                vec![0; w as usize * h as usize],
                vec![0; cw * ch],
                vec![0; cw * ch],
            ],
        )
        .unwrap()
    }

    #[test]
    fn loop_restoration_unit_count_absorbs_a_short_trailing_unit() {
        const UNIT: usize = 256;
        // Below one unit — and at zero — there is still exactly one unit.
        assert_eq!(lr_unit_count(0, UNIT), 1);
        assert_eq!(lr_unit_count(1, UNIT), 1);
        assert_eq!(lr_unit_count(256, UNIT), 1);
        // A trailing remainder under half a unit is absorbed into the previous unit...
        assert_eq!(lr_unit_count(383, UNIT), 1);
        // ...and one at or over half a unit starts its own.
        assert_eq!(lr_unit_count(384, UNIT), 2);
        assert_eq!(lr_unit_count(512, UNIT), 2);
        assert_eq!(lr_unit_count(639, UNIT), 2);
        assert_eq!(lr_unit_count(640, UNIT), 3);
        // A different unit size, so the constant is not baked into the expectations.
        assert_eq!(lr_unit_count(64, 64), 1);
        assert_eq!(lr_unit_count(96, 64), 2);
    }

    /// Geometry only — deliberately does **not** call `encode()`. The coding path is still 4:4:4
    /// (`encode_with` refuses a subsampled source), so this pins what the geometry slice actually
    /// delivers: `FrameEncoder` sizes and strides every plane independently, and `sample` clamps to
    /// each plane's own edge. Without it, every subsampling shift in the encoder is exercised only
    /// at `ss = 0`, where it is the identity and its mutants are unkillable.
    #[test]
    fn geometry_is_per_plane_for_a_subsampled_source() {
        // 17x13 luma ⇒ mi 6x4 ⇒ coded 24x16; 4:2:0 chroma is 9x7 visible on a 12x8 coded grid.
        let luma: Vec<u8> = (0..17 * 13).map(|i| (i % 251) as u8).collect();
        let chroma: Vec<u8> = (0..9 * 7).map(|i| (i % 241) as u8).collect();
        let planes = Planar8::from_planes_subsampled(
            dims(17, 13),
            gamut_color::ChromaSubsampling::Cs420,
            [luma, chroma.clone(), chroma.clone()],
        )
        .expect("valid 4:2:0 planes");
        let e = FrameEncoder::new(&planes, 40);

        assert_eq!((e.geom[0].w, e.geom[0].h), (17, 13));
        assert_eq!((e.geom[0].coded_w, e.geom[0].coded_h), (24, 16));
        assert_eq!((e.geom[1].w, e.geom[1].h), (9, 7));
        assert_eq!((e.geom[1].coded_w, e.geom[1].coded_h), (12, 8));
        assert_eq!(e.geom[1], e.geom[2]);
        // Chroma coded extent is exactly half of luma's on both axes, and each reconstruction
        // buffer is allocated at its own plane's size — not the luma one.
        assert_eq!(e.geom[1].coded_w * 2, e.geom[0].coded_w);
        assert_eq!(e.geom[1].coded_h * 2, e.geom[0].coded_h);
        for p in 0..3 {
            assert_eq!(e.recon[p].len(), e.geom[p].len(), "plane {p}");
        }

        // `sample` clamps to the *plane's* edge, not luma's: a chroma read past 9x7 replicates the
        // chroma corner. Reading with the luma extent would index a different sample entirely.
        assert_eq!(e.sample(1, 99, 99), e.sample(1, 8, 6));
        assert_eq!(e.sample(1, 8, 6), i32::from(chroma[6 * 9 + 8]));
        assert_eq!(e.sample(0, 99, 99), e.sample(0, 16, 12));
    }

    /// Builds 4:4:4 planes from a luma and a chroma generator.
    ///
    /// Both `block_is_skippable` and `decide_palette` compare a block against its `DC_PRED`, which
    /// on the lossy path reads the *reconstruction* buffer — all zeros until something is encoded.
    /// So a block qualifies only where its samples are 0 and a neighbour is available, which is why
    /// the fixtures below use 0 as the qualifying value and address blocks away from the frame edge.
    pub(super) fn planes_with(
        w: u32,
        h: u32,
        luma: impl Fn(u32, u32) -> u8,
        chroma: impl Fn(u32, u32) -> u8,
    ) -> Planar8 {
        let mut y = vec![0u8; (w * h) as usize];
        let mut c = vec![0u8; (w * h) as usize];
        for row in 0..h {
            for col in 0..w {
                let i = (row * w + col) as usize;
                y[i] = luma(col, row);
                c[i] = chroma(col, row);
            }
        }
        Planar8::from_planes(dims(w, h), [y, c.clone(), c]).expect("valid 4:4:4 planes")
    }

    /// A 4×4 mid-grey image is enough to construct an encoder; selection depends only on the
    /// residual and the quantizers, not the plane contents.
    pub(super) fn grey4x4() -> Planar8 {
        Planar8::from_rgb8_identity(&[128u8; 4 * 4 * 3], dims(4, 4)).unwrap()
    }

    /// A smooth `side`×`side` field: its quantization artefacts are what the in-loop filters
    /// remove.
    fn smooth(side: u32) -> Planar8 {
        let wave = |x: u32, y: u32| {
            let g = 128.0 + 60.0 * (f64::from(x) / 23.0).sin() * (f64::from(y) / 31.0).cos();
            g as u8
        };
        planes_with(side, side, wave, |x, y| 255 - wave(x, y))
    }

    /// [`smooth`] at one superblock.
    fn smooth_64() -> Planar8 {
        smooth(64)
    }

    /// `p` coded at `qindex` in one tile, as a frame's first pass codes it.
    fn first_pass(p: &Planar8, qindex: u8) -> (Vec<Vec<u8>>, Reconstruction) {
        let (w, h) = (p.width() as usize, p.height() as usize);
        let layout = TileLayout::resolve(crate::headers::Tiling::SINGLE, w, h, 0, None);
        FrameEncoder::new(p, qindex).encode(&layout, NonZeroUsize::MIN)
    }

    /// The partition the record `decisions` holds for the 64×64 superblock root at MI `(r, c)`.
    fn root_partition(decisions: &SbDecisions, r: usize, c: usize) -> Option<usize> {
        decisions
            .partitions
            .iter()
            .find(|d| d.at == (r, c, 64))
            .map(|d| d.bp)
    }

    #[test]
    fn each_superblock_is_recorded_at_its_own_raster_index() {
        // A 128×128 frame is a 2×2 superblock grid; superblock `i` is at MI (16 * (i / 2),
        // 16 * (i % 2)), and its record holds that root's partition and no other root's.
        let (_, first) = first_pass(&smooth(128), 110);
        assert_eq!(first.decisions.len(), 4);
        for (i, decisions) in first.decisions.iter().enumerate() {
            let (r, c) = (16 * (i / 2), 16 * (i % 2));
            assert!(root_partition(decisions, r, c).is_some(), "superblock {i}");
            assert!(!decisions.blocks.is_empty(), "superblock {i}");
            for d in &decisions.blocks {
                let (br, bc) = (d.at.0, d.at.1);
                assert!(
                    (r..r + 16).contains(&br) && (c..c + 16).contains(&bc),
                    "{:?}",
                    d.at
                );
            }
        }
    }

    #[test]
    fn a_tile_coded_on_another_thread_brings_its_record_back() {
        // Two tile columns on two threads: the right tile is coded in a clone, and its
        // superblocks' records must be merged back exactly as coding both tiles here records them.
        let p = smooth(128);
        let layout = TileLayout::resolve(crate::headers::Tiling::new(1, 0), 128, 128, 0, None);
        assert_eq!(layout.num_tiles(), 2);
        let record = |threads: usize| {
            let threads = NonZeroUsize::new(threads).unwrap();
            FrameEncoder::new(&p, 110)
                .encode(&layout, threads)
                .1
                .decisions
        };
        let (one, two) = (record(1), record(2));
        assert!(one.iter().all(|d| !d.blocks.is_empty()));
        assert_eq!(one, two);
    }

    #[test]
    fn the_chosen_filters_bring_the_frame_closer_to_the_source() {
        // On a smooth field at a coarse quantizer every stage has artefacts to remove, so the
        // choice is not "off", and what it chose leaves less error than the reconstruction the
        // filters started from.
        let p = smooth_64();
        let mut e = FrameEncoder::new(&p, 200);
        let whole = TileRect {
            r0: 0,
            r1: e.mi_rows,
            c0: 0,
            c1: e.mi_cols,
        };
        e.encode_tiles(&[whole], NonZeroUsize::MIN);
        let raw = e.recon.clone();
        let chosen = e.choose_filters(&raw);
        assert_ne!(chosen.deblock_level, 0);
        assert_ne!(chosen.cdef, CdefStrengths::default());
        let mut filtered = raw.clone();
        e.deblock(&mut filtered, chosen.deblock_level);
        let deblocked = e.frame_sse(&filtered, 0..3);
        assert!(deblocked < e.frame_sse(&raw, 0..3));
        let filtered = e.cdef(&filtered, &e.cdef_blocks(&filtered), chosen.cdef);
        assert!(e.frame_sse(&filtered, 0..1) < e.frame_sse(&raw, 0..1));
        assert!(e.frame_sse(&filtered, 0..3) < deblocked);
    }

    #[test]
    fn recoding_reconstructs_the_first_pass_and_codes_the_restoration_units() {
        // The second pass replays the first and applies its filters unchanged, so everything the
        // in-loop filters and loop restoration read is identical; only the stream gains the unit's
        // `use_wiener` and taps.
        let p = smooth_64();
        let (bytes, first) = first_pass(&p, 110);
        let layout = TileLayout::resolve(crate::headers::Tiling::SINGLE, 64, 64, 0, None);
        let lr = LrUnits {
            cols: 1,
            rows: 1,
            wiener: vec![true],
        };
        let (recoded, second) = FrameEncoder::new(&p, 110)
            .recoding(&first, lr, None)
            .encode(&layout, NonZeroUsize::MIN);
        assert!(second.planes == first.planes, "the reconstruction moved");
        assert_eq!(second.deblocked_luma, first.deblocked_luma);
        assert_eq!(second.filters, first.filters);
        assert_ne!(recoded, bytes);
    }

    #[test]
    fn recoding_codes_each_superblock_as_the_first_pass_recorded_it() {
        // Doctor the first pass's record so one superblock root takes a partition its own search
        // did not: a second pass that searched instead of replaying would choose the original
        // again. The doctored root is the last of a 2×2 grid, so the replay must also find each
        // superblock's record at its own index.
        let p = smooth(128);
        let (_, first) = first_pass(&p, 110);
        let mut record = (*first.decisions).clone();
        let root = record[3]
            .partitions
            .iter_mut()
            .find(|d| d.at == (16, 16, 64))
            .expect("the root partition is recorded");
        let doctored = if root.bp == 3 { 0 } else { 3 };
        root.bp = doctored;
        let first = Reconstruction {
            decisions: Arc::new(record),
            ..first
        };
        let layout = TileLayout::resolve(crate::headers::Tiling::SINGLE, 128, 128, 0, None);
        let lr = LrUnits {
            cols: 1,
            rows: 1,
            wiener: vec![false],
        };
        let (_, second) = FrameEncoder::new(&p, 110)
            .recoding(&first, lr, None)
            .encode(&layout, NonZeroUsize::MIN);
        assert_eq!(root_partition(&second.decisions[3], 16, 16), Some(doctored));
        // The undoctored superblocks replay as recorded.
        assert_eq!(second.decisions[..3], first.decisions[..3]);
    }
}
