//! The all-intra tile encoder: superblock/partition iteration (§5.11.2/.4), intra prediction
//! (§7.11.2), the forward transform (via `gamut-dsp`), and coefficient coding with full context
//! derivation (§5.11.39, §8.3.2). One or more uniform tiles (§5.11.1) are coded with this path.
//!
//! Two paths share this code, selected by `qindex` (`base_q_idx`):
//! - **Lossless** (`qindex == 0`): forced `TX_4X4` Walsh–Hadamard; prediction neighbours are the
//!   source samples (which equal the reconstruction under lossless); partitions are
//!   `PARTITION_NONE` except at the right/bottom frame edges.
//! - **Lossy** (`qindex > 0`): recursive `PARTITION_NONE`/`HORZ`/`VERT`/`SPLIT` down to 4×4, with
//!   per-superblock `delta_q`/`delta_lf` and optional segmentation (`SEG_LVL_ALT_Q`). Each luma
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
//!   (deblocking, CDEF, loop restoration, superres) are applied afterward in `filter`.
//!
//! The frame is coded on the MI-unit grid (`mi_cols*4 × mi_rows*4`, dimensions rounded up to a
//! multiple of 8); the out-of-frame padding is edge-replicated and cropped away on decode.
//!
//! How many of those candidates the lossy path actually tries is a [`Search`], resolved from the
//! caller's `Speed` preset; the full search is the default. The luma mode (with filter-intra), the
//! transform type, the CfL alphas and the 8×8 split are each decided by rate–distortion cost:
//! every candidate is *trialled* — reconstructed exactly as it would be coded, its symbols priced
//! from the adapting CDFs instead of coded ([`Coding::Trial`], [`Coder`]'s tally) — and the trial is
//! undone, so the encoder state the chosen candidate is then coded from is the one it was priced
//! from.

use std::num::NonZeroUsize;

use gamut_bitstream::SymbolEncoder;
use gamut_color::{ChromaSubsampling, Planar8, Planar16, clip_pixel};
use gamut_dsp::math::{round2, round2_signed};

use crate::cdf;
use crate::encoder::Speed;
use crate::geom::{PlaneGeom, has_chroma, plane_residual_size};
use crate::headers::TileLayout;
use crate::quant::{ac_q, dc_q, dequant, quantize};
use crate::transform::{TxSize, TxType, forward_transform_2d, inverse_transform_2d};

/// `NUM_BASE_LEVELS` (§3).
const NUM_BASE_LEVELS: i32 = 2;
/// `NUM_BASE_LEVELS + COEFF_BASE_RANGE`, the golomb threshold (§5.11.39).
const COEFF_BASE_PLUS_RANGE: i32 = 14;

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

/// The eight directional luma modes, in `YMode` order (`V`..`D67`), used to drive the ≥8×8 angle
/// search. Each is a candidate base angle that `AngleDeltaY ∈ [-3, 3]` fine-tunes.
const DIRECTIONAL_MODES: [u8; 8] = [
    V_PRED, H_PRED, D45_PRED, D135_PRED, D113_PRED, D157_PRED, D203_PRED, D67_PRED,
];

/// Every `AngleDeltaY` (§5.11.42), in the order the full search tries them.
const ALL_ANGLE_DELTAS: [i8; 7] = [-3, -2, -1, 0, 1, 2, 3];

/// Every `CflAlpha` magnitude (§5.11.45), each tried with both signs.
const ALL_CFL_MAGNITUDES: [i32; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// The `TX_SET_INTRA_2` types tried after `DCT_DCT`, with their `Tx_Type_Intra_Inv_Set2` symbols,
/// in the order the full search has always tried them (which decides ties): a pruned search keeps
/// a prefix, so `ADST_ADST` is the one extra type a two-type search tries.
const TX_TYPE_TRIALS: [(TxType, usize); 4] = [
    (TxType::AdstAdst, 2),
    (TxType::AdstDct, 3),
    (TxType::DctAdst, 4),
    (TxType::Idtx, 0),
];

/// The mode-decision search a frame runs: a [`Speed`] preset resolved into per-tool candidate
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
    /// The square sizes `decide_rect` may code as two rectangular halves.
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
    /// The search a [`Speed`] preset runs. The table is documented on [`Speed`].
    pub(crate) fn for_speed(speed: Speed) -> Self {
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
        match speed {
            Speed::Thorough => thorough,
            Speed::Balanced => balanced,
            Speed::Fast => fast,
            Speed::Fastest => Self {
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

/// `Filter_Intra_Mode_To_Intra_Dir[INTRA_FILTER_MODES]` (§9.3): maps a filter-intra mode to the
/// `intraDir` used to select the 16×16 transform-type CDF (§9.4). Order:
/// `{FILTER_DC, FILTER_V, FILTER_H, FILTER_D157, FILTER_PAETH}`.
const FILTER_INTRA_MODE_TO_INTRA_DIR: [u8; 5] = [DC_PRED, V_PRED, H_PRED, D157_PRED, DC_PRED];

/// True for the eight directional intra modes (`V_PRED..=D67_PRED`, mode indices `1..=8`), which is
/// `is_directional_mode` (§7.11.2). These are the modes for which `angle_delta_y` is signaled (≥8×8)
/// and whose prediction follows the directional process (§7.11.2.4).
const fn is_directional(mode: u8) -> bool {
    V_PRED <= mode && mode <= D67_PRED
}

/// A selected luma palette for a block (§5.11.46/.49): the sorted distinct colors and the per-pixel
/// `ColorMapY` (index into `colors`, row-major over the block).
struct PaletteBlock {
    /// Palette entries, sorted ascending, at the frame's coded bit depth.
    colors: Vec<u16>,
    index_map: Vec<u8>,
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

/// The largest luma palette AV1 allows (§5.11.46: `PaletteSizeY` is 2..=8), and therefore the
/// capacity of both the palette itself and the neighbour cache entries that carry it.
const PALETTE_MAX: usize = 8;

/// `paletteBits` after coding palette colour `colour` (§5.11.46): the delta width narrows to what
/// the range still above that colour, `(1 << BitDepth) - colour - 1`, could ever need.
///
/// Extracted because in place it is invisible: a too-wide `paletteBits` still round-trips through
/// the decoder for every palette whose later deltas happen to fit, so only the boundary colours —
/// where `CeilLog2` changes — distinguish the right arithmetic from the wrong.
fn palette_delta_bits(bit_depth: u32, colour: u16, current: u32) -> u32 {
    let range = (1 << bit_depth) - i32::from(colour) - 1;
    current.min(ceil_log2(range.max(1) as usize))
}

/// `CeilLog2(x)` (§4.7): the smallest `k` with `2^k >= x` (0 for `x <= 1`).
fn ceil_log2(x: usize) -> u32 {
    if x < 2 { 0 } else { (x - 1).ilog2() + 1 }
}

/// `get_palette_color_context` (§5.11.50): from the three already-decoded neighbours of `(r, c)` in
/// the `bw`-wide `color_map`, computes the reordered `ColorOrder` (most-likely color first) and the
/// color context. `n` is the palette size.
fn palette_color_context(
    color_map: &[u8],
    bw: usize,
    r: usize,
    c: usize,
    n: usize,
) -> ([usize; 8], usize) {
    let mut scores = [0i32; 8];
    let mut order = [0usize; 8];
    for (i, o) in order.iter_mut().enumerate() {
        *o = i;
    }
    if c > 0 {
        scores[color_map[r * bw + (c - 1)] as usize] += 2;
    }
    if r > 0 && c > 0 {
        scores[color_map[(r - 1) * bw + (c - 1)] as usize] += 1;
    }
    if r > 0 {
        scores[color_map[(r - 1) * bw + c] as usize] += 2;
    }
    // Partial selection sort of the first PALETTE_NUM_NEIGHBORS = 3 by descending score, rotating the
    // chosen entry into place.
    for i in 0..3 {
        // Running argmax over `i..n` (the `j == i` step is a no-op since `max_idx` starts at `i`).
        let mut max_idx = i;
        for j in i..n {
            if scores[j] > scores[max_idx] {
                max_idx = j;
            }
        }
        if max_idx != i {
            let (ms, mo) = (scores[max_idx], order[max_idx]);
            let mut k = max_idx;
            while k > i {
                scores[k] = scores[k - 1];
                order[k] = order[k - 1];
                k -= 1;
            }
            scores[i] = ms;
            order[i] = mo;
        }
    }
    let hash = (scores[0] + scores[1] * 2 + scores[2] * 2) as usize;
    (order, cdf::PALETTE_COLOR_CONTEXT[hash] as usize)
}

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

/// The square transform size of the given side length (4/8/16/32). Used to map a `bw >> tx_depth`
/// width back to a [`TxSize`] under `TX_MODE_SELECT`.
const fn square_tx(width: usize) -> TxSize {
    match width {
        4 => TxSize::Tx4x4,
        8 => TxSize::Tx8x8,
        16 => TxSize::Tx16x16,
        32 => TxSize::Tx32x32,
        _ => TxSize::Tx64x64,
    }
}

/// Whether a square block of `num4x4 × num4x4` MI cells at MI position `(r, c)` overhangs the
/// `mi_rows × mi_cols` frame, i.e. does not fully fit. AV1 still codes such a block (a
/// `PARTITION_NONE` is allowed when only the top/left half is in-frame), but the lossy
/// reconstruction buffer is the MI-frame size, so an overhanging block must instead be force-split
/// (see [`FrameEncoder::encode_partition`]). A block exactly reaching an edge (`r + num4x4 ==
/// mi_rows`) fits and is not flagged.
const fn block_exceeds_frame(
    r: usize,
    c: usize,
    num4x4: usize,
    mi_rows: usize,
    mi_cols: usize,
) -> bool {
    r + num4x4 > mi_rows || c + num4x4 > mi_cols
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

fn max_tx_size_rect(w: usize, h: usize) -> TxSize {
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
        // AV1 has no block whose aspect ratio exceeds 4:1, so no other pair is constructible.
        // Coding one against a mismatched transform would desynchronise the decoder silently, so
        // fail here instead.
        _ => panic!("no transform matches this block shape"),
    }
}

/// How a single transform block's samples are predicted. `mode` is the intra mode (`DC_PRED` for
/// chroma); `angle_delta` is the directional fine angle (`AngleDeltaY`, signaled only for ≥8×8 luma
/// directional blocks); `filter_intra` overrides luma prediction with a recursive filter-intra mode
/// (§7.11.2.3); `cfl_alpha` adds the chroma-from-luma high-frequency term (§7.11.5). At most one of
/// the latter two is set, and only on its respective plane.
#[derive(Clone, Copy)]
struct Pred {
    mode: u8,
    angle_delta: i8,
    filter_intra: Option<u8>,
    cfl_alpha: Option<i32>,
}

/// Number of loop-restoration units spanning `size` samples on a `unit`-sample grid (§5.11.57).
///
/// A trailing partial unit smaller than half a unit is absorbed into the previous one rather than
/// starting its own, hence the `+ unit / 2` before the divide; a frame smaller than one unit still
/// has one. Extracted from `write_lr` so the rounding is directly testable: gamut's unit grid is
/// 256 samples and every fixture is smaller than that, so in place this arithmetic is pinned at
/// exactly one point (`1`) and any mutation of it survives.
fn lr_unit_count(size: usize, unit: usize) -> usize {
    ((size + unit / 2) / unit).max(1)
}

/// The reconstructed (decoded) planes on the coded MI grid, used as prediction neighbours in the
/// lossy path and exported for the bit-exact decoder cross-check.
pub(crate) struct Reconstruction {
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
    /// Whether `delta_q_present` (lossy path); when set, the first block of each superblock signals a
    /// `delta_q` and `read_deltas` gates it.
    delta_q_present: bool,
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

/// Row stride of [`FrameEncoder::block_decoded`]: a 64×64 superblock is 16 MI wide, plus the `-1`
/// guard row/column and the `+16` below/right edge ⇒ indices `-1..=16` → 18 entries.
const BD_STRIDE: usize = 18;

/// `Partition_Context` lookup (`dav1d_al_part_ctx[2][N_BL_LEVELS][PARTITION]`, §5.11.4), restricted to
/// the partition types this encoder emits: `[above|left][block level 0=128…4=8][NONE|HORZ|VERT|SPLIT]`.
/// After a terminal partition, the block's MI cells take these context bytes; a later block reads bit
/// `bsl - 1` of the neighbour byte to form its partition context. The SPLIT column is only ever read at
/// 8×8 (`bl == 4`) — at coarser levels a split recurses and the column (0) is unused.
const AL_PART_CTX: [[[u8; 4]; 5]; 2] = [
    [
        [0x00, 0x00, 0x10, 0x00],
        [0x10, 0x10, 0x18, 0x00],
        [0x18, 0x18, 0x1c, 0x00],
        [0x1c, 0x1c, 0x1e, 0x00],
        [0x1e, 0x1e, 0x1f, 0x1f],
    ],
    [
        [0x00, 0x10, 0x00, 0x00],
        [0x10, 0x18, 0x10, 0x00],
        [0x18, 0x1c, 0x18, 0x00],
        [0x1c, 0x1e, 0x1c, 0x00],
        [0x1e, 0x1f, 0x1e, 0x1f],
    ],
];

/// `Sm_Weights` (§9.3) for a transform dimension `size` (8/16/32/64). The SMOOTH predictors weight
/// each axis independently, so a rectangular block reads a different-length table per axis.
fn sm_weights(size: usize) -> &'static [i32] {
    match size {
        8 => &cdf::SM_WEIGHTS_8X8,
        16 => &cdf::SM_WEIGHTS_16X16,
        32 => &cdf::SM_WEIGHTS_32X32,
        _ => &cdf::SM_WEIGHTS_64X64,
    }
}

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
            PlaneSource::Eight([planes.plane(0), planes.plane(1), planes.plane(2)]),
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
            PlaneSource::High([planes.plane(0), planes.plane(1), planes.plane(2)]),
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
            delta_q_present: qindex > 0,
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
            search: Search::for_speed(Speed::default()),
        }
    }

    /// Replaces the mode-decision search (the full one by default).
    pub(crate) fn with_search(mut self, search: Search) -> Self {
        self.search = search;
        self
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

    /// Signals the loop-restoration units (§5.11.57 `read_lr`) whose top-left falls in the superblock
    /// at MI `(r, c)`. Luma uses `RESTORE_WIENER` with the default reference filter — every coded
    /// coefficient delta is zero — and chroma is `RESTORE_NONE`. The unit grid is 256×256
    /// (`lr_unit_shift = 2`); the same filter is later applied by [`crate::filter::loop_restore_wiener_luma`].
    fn write_lr(&mut self, r: usize, c: usize) {
        const UNIT: usize = 256;
        // (offset, num_syms, k) for the three Wiener half-taps (§5.11.58 read_lr_unit).
        const TAPS: [(i32, i32, u32); 3] = [(5, 16, 1), (23, 32, 2), (17, 64, 3)];
        // Luma geometry deliberately: loop restoration signals `RESTORE_NONE` for both chroma
        // planes, so the unit grid is defined on luma alone (§5.11.57).
        let unit_cols = lr_unit_count(self.geom[0].w, UNIT);
        let unit_rows = lr_unit_count(self.geom[0].h, UNIT);
        let row_start = (r * 4).div_ceil(UNIT);
        let row_end = unit_rows.min(((r + 16) * 4).div_ceil(UNIT));
        let col_start = (c * 4).div_ceil(UNIT);
        let col_end = unit_cols.min(((c + 16) * 4).div_ceil(UNIT));
        for _ in row_start..row_end {
            for _ in col_start..col_end {
                // restore_wiener = 1 (use Wiener), then vertical then horizontal half-taps.
                self.sym.encode(1, self.cdfs.restore_wiener.slot());
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
                // ReadDeltas is armed per superblock (§7.18); the first block then signals delta_q.
                self.read_deltas = self.delta_q_present;
                self.clear_block_decoded(r, c);
                if self.qindex > 0 {
                    self.write_lr(r, c);
                }
                self.encode_partition(r, c, 64);
            }
        }
        std::mem::take(&mut self.sym.range).finish()
    }

    /// Copies `tile`'s rectangle of what the in-loop filters read after the tiles are coded — the
    /// reconstruction and the per-MI maps `filter::deblock` and `filter::cdef` take — from `from`,
    /// an encoder that coded that tile, into `self`.
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
        let mut planes = self.recon;
        let mut deblocked_luma = Vec::new();
        if self.qindex > 0 {
            crate::filter::deblock(
                &mut planes,
                &self.geom,
                self.subsampling,
                self.mi_cols,
                &self.tx_log2,
                &self.tx_log2_h,
                &self.mi_bsl,
                &self.mi_bsl_h,
                &self.mi_dlf,
                self.qindex,
                self.bit_depth,
            );
            // CDEF reads the deblocked reconstruction and produces a deringed one (§7.15). The
            // deblocked luma is retained for the loop-restoration stripe boundaries (§7.17); loop
            // restoration itself is applied by the caller after the optional superres upscale.
            deblocked_luma = planes[0].clone();
            planes = crate::filter::cdef(
                &planes,
                &self.geom,
                &self.mi_skip,
                self.mi_cols,
                self.qindex,
                self.num_planes,
                self.bit_depth,
            );
        }
        let recon = Reconstruction {
            planes,
            geom: self.geom,
            bit_depth: self.bit_depth,
            deblocked_luma,
        };
        (tile_bytes, recon)
    }

    /// Padded (edge-replicated) source sample of `plane` at coded-grid position `(x, y)`.
    fn sample(&self, plane: usize, x: usize, y: usize) -> i32 {
        let g = self.geom[plane];
        let xx = x.min(g.w - 1);
        let yy = y.min(g.h - 1);
        self.planes.at(plane, yy * g.w + xx)
    }

    fn encode_partition(&mut self, r: usize, c: usize, bw: usize) {
        if r >= self.mi_rows || c >= self.mi_cols {
            return;
        }
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let bsl = num4x4.trailing_zeros() as usize; // Mi_Width_Log2

        // `bp`: the chosen partition (0 NONE, 1 HORZ, 2 VERT, 3 SPLIT; `usize::MAX` = no partition
        // decoded, for the forced-NONE sub-8×8 blocks).
        let bp = if bw < 8 {
            usize::MAX
        } else if has_rows && has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            if self.qindex == 0 {
                self.sym.encode(0, self.cdfs.partition(bsl, ctx)); // PARTITION_NONE
                0
            } else if block_exceeds_frame(r, c, num4x4, self.mi_rows, self.mi_cols) {
                // The block extends past the frame edge (only its top/left half is in-frame, which is
                // why `has_rows && has_cols` still hold). AV1 permits a frame-spanning PARTITION_NONE
                // here, but the lossy reconstruction buffer is the MI-frame size, so a single
                // block-size transform would write out of bounds. Force PARTITION_SPLIT — always a
                // legal choice in this branch — so the recursion descends until every coded transform
                // fits (a 4×4 leaf always fits); the out-of-frame leaves are skipped in `residual`.
                self.sym.encode(3, self.cdfs.partition(bsl, ctx)); // PARTITION_SPLIT
                3
            } else if let Some(d) = self.decide_rect(r, c, bw) {
                // PARTITION_HORZ (1) / PARTITION_VERT (2): two rectangular halves, each its own mode
                // and one matching rectangular transform.
                self.sym.encode(d, self.cdfs.partition(bsl, ctx));
                d
            } else if (8..=64).contains(&bw) && !self.choose_split(r, c, bw, ctx) {
                // Lossy: an 8×8…64×64 region coded as a single PARTITION_NONE block + one
                // TX_8X8…TX_64X64 when it is smooth enough; otherwise split for per-block
                // mode/transform adaptation (each SPLIT halves, recursing down to 4×4 as needed,
                // or until the block is smaller than the search's `split_from`).
                self.sym.encode(0, self.cdfs.partition(bsl, ctx)); // PARTITION_NONE
                0
            } else {
                self.sym.encode(3, self.cdfs.partition(bsl, ctx)); // PARTITION_SPLIT
                3
            }
        } else if has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            let cdf2 = split_or_horz_cdf(self.cdfs.partition_probs(bsl, ctx));
            self.sym.encode_symbol(1, &cdf2); // split
            3
        } else if has_rows {
            let ctx = self.partition_ctx(r, c, bsl);
            let cdf2 = split_or_vert_cdf(self.cdfs.partition_probs(bsl, ctx));
            self.sym.encode_symbol(1, &cdf2); // split
            3
        } else {
            3 // forced PARTITION_SPLIT, no symbol
        };

        // AbovePartitionContext/LeftPartitionContext update (§5.11.4): a terminal partition (NONE/
        // HORZ/VERT) writes its level's context; PARTITION_SPLIT writes only at 8×8 (its 4×4 children
        // decode no partition), otherwise its children update as they recurse. The forced-NONE sub-8×8
        // case (`bp = MAX`) decodes no partition, so it writes nothing.
        if bw >= 8 && bp != usize::MAX && (bp != 3 || bsl == 1) {
            self.update_partition_ctx(r, c, bsl, bp);
        }

        match bp {
            usize::MAX | 0 => self.encode_block(r, c, bw, bw),
            1 => {
                // PARTITION_HORZ: top then bottom half, each `bw × bw/2`.
                self.encode_block(r, c, bw, bw / 2);
                if r + half < self.mi_rows {
                    self.encode_block(r + half, c, bw, bw / 2);
                }
            }
            2 => {
                // PARTITION_VERT: left then right half, each `bw/2 × bw`.
                self.encode_block(r, c, bw / 2, bw);
                if c + half < self.mi_cols {
                    self.encode_block(r, c + half, bw / 2, bw);
                }
            }
            _ => {
                let h = bw / 2;
                self.encode_partition(r, c, h);
                self.encode_partition(r, c + half, h);
                self.encode_partition(r + half, c, h);
                self.encode_partition(r + half, c + half, h);
            }
        }
    }

    fn partition_ctx(&self, r: usize, c: usize, bsl: usize) -> usize {
        // §8.3.2: `above`/`left` are bit `4 - bl == bsl - 1` of the neighbour partition context.
        let bit = bsl - 1;
        // A tile's top edge has no above neighbour (above_partition holds the tile row above's value).
        let above = if r > self.tile_r0 {
            (self.above_partition[c] >> bit) & 1
        } else {
            0
        };
        // A tile's left edge has no left neighbour (left_partition holds the adjacent tile's value).
        let left = if c > self.tile_c0 {
            (self.left_partition[r] >> bit) & 1
        } else {
            0
        };
        usize::from(left) * 2 + usize::from(above)
    }

    /// Updates `AbovePartitionContext`/`LeftPartitionContext` over the `n4 × n4` MI cells of a block
    /// coded with terminal partition `bp` (0 = NONE, 1 = HORZ, 2 = VERT) at level `bsl`
    /// (`Mi_Width_Log2`). `PARTITION_SPLIT` does not update — its children write their own context.
    fn update_partition_ctx(&mut self, r: usize, c: usize, bsl: usize, bp: usize) {
        let bl = 5 - bsl; // block level: BL_128X128=0 … BL_8X8=4
        let n4 = 1usize << bsl;
        let (a, l) = (AL_PART_CTX[0][bl][bp], AL_PART_CTX[1][bl][bp]);
        for k in 0..n4 {
            if c + k < self.mi_cols {
                self.above_partition[c + k] = a;
            }
            if r + k < self.mi_rows {
                self.left_partition[r + k] = l;
            }
        }
    }

    /// Decides whether the fully on-screen lossy `bw × bw` region at MI `(r, c)`, whose partition
    /// symbol is coded under context `ctx`, is coded `PARTITION_SPLIT` rather than
    /// `PARTITION_NONE`. Never below the search's `split_from`. An 8×8 region — whose children
    /// are 4×4 leaves, so both alternatives are a few blocks to code — is decided by
    /// rate–distortion cost: each alternative is trialled whole, partition symbol, mode info,
    /// every plane's residual, and the cheaper taken (`PARTITION_NONE` on a tie). A larger region
    /// is decided by [`Self::should_split`]'s texture test, since trialling it would mean
    /// trialling every partition beneath it.
    fn choose_split(&mut self, r: usize, c: usize, bw: usize, ctx: usize) -> bool {
        if bw < self.search.split_from {
            return false;
        }
        if bw != 8 {
            return self.should_split(r, c, bw);
        }
        // `Mi_Width_Log2` of an 8×8 block.
        let bsl = 1;
        let none = self.trial_cost(r, c, bw, |e| {
            e.sym.encode(0, e.cdfs.partition(bsl, ctx));
            e.update_partition_ctx(r, c, bsl, 0);
            e.encode_block(r, c, bw, bw);
        });
        let split = self.trial_cost(r, c, bw, |e| {
            e.sym.encode(3, e.cdfs.partition(bsl, ctx));
            e.update_partition_ctx(r, c, bsl, 3);
            for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                e.encode_partition(r + dr, c + dc, 4);
            }
        });
        split < none
    }

    /// The rate–distortion cost of coding the `bw × bw` region at MI `(r, c)` as `code` does,
    /// with everything coding it changed put back afterwards: `code` runs with the tile's coder
    /// tallying rate instead of coding (so neither the stream nor the CDFs move), then the
    /// region's squared error over every plane is weighed against that rate by
    /// [`Self::rd_cost`], and the region's reconstruction, contexts and per-MI state, and the
    /// quantizer state, are restored.
    fn trial_cost(&mut self, r: usize, c: usize, bw: usize, code: impl FnOnce(&mut Self)) -> u64 {
        let saved = self.save_block(r, c, bw);
        let outer = self.sym.tally.replace(0);
        code(self);
        let rate = std::mem::replace(&mut self.sym.tally, outer).unwrap_or(0);
        let sse = saved.planes.iter().map(|s| self.region_sse(s)).sum();
        self.restore_block(saved);
        self.rd_cost(sse, rate)
    }

    /// Captures everything coding the `bw × bw` region at MI `(r, c)` can change, for
    /// [`Self::trial_cost`] to put back.
    fn save_block(&self, r: usize, c: usize, bw: usize) -> BlockState {
        let n4 = bw / 4;
        let rows = r..(r + n4).min(self.mi_rows);
        let cols = c..(c + n4).min(self.mi_cols);
        let region = |map: &[u8]| -> Vec<u8> {
            rows.clone()
                .flat_map(|rr| {
                    map[rr * self.mi_cols + cols.start..rr * self.mi_cols + cols.end]
                        .iter()
                        .copied()
                })
                .collect()
        };
        BlockState {
            r,
            c,
            w4: cols.len(),
            maps: self.mi_maps().map(|m| region(m)),
            pcolors: rows
                .clone()
                .flat_map(|rr| {
                    self.mi_pcolors[rr * self.mi_cols + cols.start..rr * self.mi_cols + cols.end]
                        .iter()
                        .copied()
                })
                .collect(),
            dlf: rows
                .clone()
                .flat_map(|rr| {
                    self.mi_dlf[rr * self.mi_cols + cols.start..rr * self.mi_cols + cols.end]
                        .iter()
                        .copied()
                })
                .collect(),
            above_partition: self.above_partition.clone(),
            left_partition: self.left_partition.clone(),
            planes: (0..self.num_planes)
                .map(|p| self.save_plane(p, r, c, bw, bw))
                .collect(),
            quant: (
                self.current_qindex,
                self.current_dlf,
                self.dc_quant,
                self.ac_quant,
                self.read_deltas,
            ),
        }
    }

    /// Puts back what [`Self::save_block`] captured.
    fn restore_block(&mut self, s: BlockState) {
        let cols = self.mi_cols;
        let w4 = s.w4;
        for (map, saved) in self.mi_maps_mut().into_iter().zip(&s.maps) {
            for (i, row) in saved.chunks_exact(w4.max(1)).enumerate() {
                let at = (s.r + i) * cols + s.c;
                map[at..at + row.len()].copy_from_slice(row);
            }
        }
        for (i, row) in s.pcolors.chunks_exact(w4.max(1)).enumerate() {
            let at = (s.r + i) * cols + s.c;
            self.mi_pcolors[at..at + row.len()].copy_from_slice(row);
        }
        for (i, row) in s.dlf.chunks_exact(w4.max(1)).enumerate() {
            let at = (s.r + i) * cols + s.c;
            self.mi_dlf[at..at + row.len()].copy_from_slice(row);
        }
        self.above_partition = s.above_partition;
        self.left_partition = s.left_partition;
        // Every plane's state was captured before any was coded, so the `BlockDecoded` map and
        // CfL bounds each one also carries are the same pre-trial values.
        for plane in s.planes {
            self.restore_plane(plane);
        }
        (
            self.current_qindex,
            self.current_dlf,
            self.dc_quant,
            self.ac_quant,
            self.read_deltas,
        ) = s.quant;
    }

    /// The per-MI `u8` maps coding a block writes, in a fixed order.
    fn mi_maps(&self) -> [&Vec<u8>; 8] {
        [
            &self.mi_bsl,
            &self.mi_bsl_h,
            &self.mi_ymode,
            &self.mi_skip,
            &self.mi_segid,
            &self.mi_psize,
            &self.tx_log2,
            &self.tx_log2_h,
        ]
    }

    /// [`Self::mi_maps`], mutably.
    fn mi_maps_mut(&mut self) -> [&mut Vec<u8>; 8] {
        [
            &mut self.mi_bsl,
            &mut self.mi_bsl_h,
            &mut self.mi_ymode,
            &mut self.mi_skip,
            &mut self.mi_segid,
            &mut self.mi_psize,
            &mut self.tx_log2,
            &mut self.tx_log2_h,
        ]
    }

    /// Decides whether the lossy `bw × bw` region at MI `(r, c)` should split rather than be coded as
    /// a single `PARTITION_NONE` block. A textured region (large luma range over the source) splits so
    /// each sub-block can pick its own mode and transform; a smooth region stays whole, where one
    /// prediction and a single larger transform code it more compactly. This is a quality decision —
    /// either choice reconstructs bit-exactly — so the threshold is a deterministic placeholder
    /// (rate-distortion partition search is deferred).
    fn should_split(&self, r: usize, c: usize, bw: usize) -> bool {
        let n4 = bw / 4;
        // A 64×64 block that extends past the frame edge is split: the encoder only codes a
        // `PARTITION_NONE` 64×64 block when the whole superblock is on-screen (a partial 64×64 block
        // is valid but adds no value, and the smaller sub-blocks crop cleanly).
        if bw == 64 && (r + n4 > self.mi_rows || c + n4 > self.mi_cols) {
            return true;
        }
        // A palette-able block (few luma colors + flat chroma) is kept whole so it can use palette
        // mode, even though its luma range is large — splitting it would forfeit the palette coding.
        if self.decide_palette(r, c, bw).is_some() {
            return false;
        }
        self.luma_range(c * 4, r * 4, bw, bw) > 32
    }

    /// Updates the active dequantizers to `CurrentQIndex` — used when a `delta_q` changes it. Only the
    /// `dc_quant`/`ac_quant` step sizes follow `CurrentQIndex`; the coefficient-CDF context `qctx`
    /// stays at its frame value (`init_coeff_cdfs` derives it from `base_q_idx`, §8.3.2), so it is
    /// deliberately not touched here.
    fn set_quant(&mut self, qindex: i32) {
        self.current_qindex = qindex;
        self.dc_quant = dc_q(self.bit_depth, qindex);
        self.ac_quant = ac_q(self.bit_depth, qindex);
    }

    /// Sets the active dc/ac quantizers for a block to `get_qindex(segment)` (§7.12.2): the segment's
    /// `SEG_LVL_ALT_Q` delta is added to `CurrentQIndex` (the delta-Q accumulator, left unchanged), so
    /// each block can quantize at its own step size.
    fn apply_seg_quant(&mut self, segid: usize) {
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
    /// `CurrentQIndex = Clip3(1, 255, CurrentQIndex + delta)` (with `delta_q_res = 0`). The delta is a
    /// quality placeholder (any value reconstructs bit-exactly) chosen to oscillate around the frame
    /// quantizer so adjacent superblocks differ. Magnitude is kept ≤ 2, so the `DELTA_Q_SMALL` escape
    /// is never coded.
    fn signal_delta_q(&mut self) {
        let sb_idx = self.sb_r / 16 + self.sb_c / 16;
        let delta: i32 = match sb_idx % 3 {
            0 => 1,
            1 => -1,
            _ => 2,
        };
        let abs = delta.unsigned_abs() as usize;
        self.sym.encode(abs, self.cdfs.delta_q.slot());
        self.sym.encode_literal(u32::from(delta < 0), 1); // delta_q_sign_bit (1 ⇒ negative)
        let nq = (self.current_qindex + delta).clamp(1, 255);
        self.set_quant(nq);
    }

    /// `read_delta_lf` (§5.11.13, `delta_lf_multi = 0`) for the first block of a superblock: signals a
    /// small per-SB `delta_lf` (magnitude under `Default_Delta_Lf_Cdf`, then a sign bit when non-zero)
    /// and updates `CurrentDeltaLF = Clip3(-MAX_LOOP_FILTER, MAX_LOOP_FILTER, CurrentDeltaLF + delta)`.
    /// The delta is a non-negative `{0, 1}` placeholder (so the per-superblock loop-filter level only
    /// rises above the frame level — `loop_filter_level + DeltaLF` stays positive and the p-side-level
    /// fallback for a zero level is never needed). When the frame level is 0 the delta is forced to 0
    /// (the deblock stays disabled). Magnitude ≤ 2 ⇒ the `DELTA_LF_SMALL` escape is never coded.
    fn signal_delta_lf(&mut self) {
        let lf = i32::from(crate::filter::deblock_level(self.qindex));
        let delta: i32 = if lf == 0 {
            0
        } else {
            (self.sb_r / 16 + self.sb_c / 16) as i32 % 2
        };
        let abs = delta.unsigned_abs() as usize;
        self.sym.encode(abs, self.cdfs.delta_lf.slot());
        if abs > 0 {
            self.sym.encode_literal(u32::from(delta < 0), 1); // delta_lf_sign_bit
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
    /// same samples (prediction), so signalling `skip` is a lossless bit saving. The luma mode a flat
    /// block selects is `DC_PRED` with no CfL / filter-intra, so the predicted samples equal the
    /// source — the chosen `skip` is therefore bit-exact regardless of the (deterministic) mode.
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

    /// Decides whether to code a lossy block at MI `(r, c)` with luma palette mode (§5.11.46): the
    /// luma block must have 2..=8 distinct colors, and (so the block can be coded `skip = 1` with the
    /// reconstruction being exactly the palette + DC chroma) every **coded** chroma plane must be
    /// flat and DC-predictable — a monochrome frame has none, so that condition is vacuous there
    /// and palette is reachable on content a 4:4:4 frame would reject. Returns the sorted palette
    /// and the per-pixel index map. This is a quality decision; any choice reconstructs
    /// bit-exactly, so only the signaling has to be correct.
    fn decide_palette(&self, r: usize, c: usize, bw: usize) -> Option<PaletteBlock> {
        // A pruned search does not look for a palette at all — which also stops `should_split`
        // keeping a palette-able region whole for one.
        if !self.search.palette {
            return None;
        }
        let (sx, sy) = (c * 4, r * 4);
        // Distinct luma colors, kept sorted as they are found. A presence array would have to be
        // `1 << BitDepth` entries and reallocated per candidate block; the palette can hold at most
        // 8 colours, so an insertion-sorted 8-entry vector is both smaller and an earlier bail-out.
        let mut colors: Vec<u16> = Vec::with_capacity(PALETTE_MAX);
        for i in 0..bw {
            for j in 0..bw {
                let v = self.sample(0, sx + j, sy + i) as u16;
                if let Err(pos) = colors.binary_search(&v) {
                    if colors.len() == PALETTE_MAX {
                        return None; // more than 8 distinct colours
                    }
                    colors.insert(pos, v);
                }
            }
        }
        if colors.len() < 2 {
            return None;
        }
        // Chroma must be flat and exactly DC-predictable (chroma residual identically 0 ⇒ skip = 1).
        // A monochrome frame has no chroma to constrain, so the loop is empty and every luma
        // palette candidate stands on its own.
        for plane in 1..self.num_planes {
            // The plane's own residual block, not the luma extent at the luma position.
            let Some((px, py, pw, ph)) = self.plane_block(r, c, bw, bw, plane) else {
                continue;
            };
            let v0 = self.sample(plane, px, py);
            for i in 0..ph {
                for j in 0..pw {
                    if self.sample(plane, px + j, py + i) != v0 {
                        return None;
                    }
                }
            }
            if self.dc_pred(plane, px, py, pw, ph) != v0 {
                return None;
            }
        }
        let mut index_map = vec![0u8; bw * bw];
        for i in 0..bw {
            for j in 0..bw {
                let v = self.sample(0, sx + j, sy + i) as u16;
                index_map[i * bw + j] = colors.binary_search(&v).unwrap_or(0) as u8;
            }
        }

        Some(PaletteBlock { colors, index_map })
    }

    /// `get_palette_cache` (§5.11.46) for luma: the sorted, deduplicated merge of the above (only
    /// when the block is not at the top of its 64-superblock) and left blocks' palettes.
    fn palette_cache(&self, r: usize, c: usize) -> Vec<u16> {
        let above_n = if !r.is_multiple_of(16) {
            self.mi_psize[(r - 1) * self.mi_cols + c] as usize
        } else {
            0
        };
        let left_n = if c > self.tile_c0 {
            self.mi_psize[r * self.mi_cols + (c - 1)] as usize
        } else {
            0
        };
        let blank = [0u16; 8];
        let above = if above_n > 0 {
            &self.mi_pcolors[(r - 1) * self.mi_cols + c]
        } else {
            &blank
        };
        let left = if left_n > 0 {
            &self.mi_pcolors[r * self.mi_cols + (c - 1)]
        } else {
            &blank
        };
        let mut cache: Vec<u16> = Vec::new();
        let push = |cache: &mut Vec<u16>, v: u16| {
            if cache.last() != Some(&v) {
                cache.push(v);
            }
        };
        let (mut ai, mut li) = (0, 0);
        while ai < above_n && li < left_n {
            let (ac, lc) = (above[ai], left[li]);
            if lc < ac {
                push(&mut cache, lc);
                li += 1;
            } else {
                push(&mut cache, ac);
                ai += 1;
                if lc == ac {
                    li += 1;
                }
            }
        }
        while ai < above_n {
            push(&mut cache, above[ai]);
            ai += 1;
        }
        while li < left_n {
            push(&mut cache, left[li]);
            li += 1;
        }
        cache
    }

    /// `ns(n)` literal coding (§4.10.7) of `val ∈ [0, n)`.
    fn encode_ns(&mut self, val: usize, n: usize) {
        if n <= 1 {
            return;
        }
        let w = n.ilog2() + 1;
        let m = (1usize << w) - n;
        if val < m {
            self.sym.encode_literal(val as u32, w - 1);
        } else {
            let coded = val + m;
            self.sym.encode_literal((coded >> 1) as u32, w - 1);
            self.sym.encode_literal((coded & 1) as u32, 1);
        }
    }

    /// Signals the luma palette colors (§5.11.46): the cache-usage flags (all 0 — the encoder reuses
    /// no cached color) followed by every palette color as new — the first raw, the rest delta-coded.
    fn signal_palette_colors(&mut self, colors: &[u16], cache_n: usize) {
        // use_palette_color_cache_y flags: 0 for each cached color (none reused), while
        // `idx < PaletteSizeY`. Since none are used, `idx` stays 0 and all `cache_n` flags are
        // emitted.
        for _ in 0..cache_n {
            self.sym.encode_literal(0, 1);
        }
        // New colors: the first is raw `L(BitDepth)`; the rest are positive deltas coded in
        // `palette_bits = BitDepth - 3 + palette_num_extra_bits_y`. Split rather than indexed, so
        // the "is there a delta at all" question is the slice's own emptiness — `psize` never being
        // 1 (§5.11.46 bounds it at 2..=8, and `decide_palette` enforces that) would otherwise leave
        // a numeric bound here that no reachable palette can exercise.
        let Some((&first, rest)) = colors.split_first() else {
            return;
        };
        self.sym.encode_literal(u32::from(first), self.bit_depth);
        if rest.is_empty() {
            return;
        }
        // palette_num_extra_bits_y = 3 ⇒ palette_bits = BitDepth, which always holds a delta: the
        // colours are sorted and distinct, so `delta - 1 < 1 << BitDepth`.
        self.sym.encode_literal(3, 2);
        let mut palette_bits = self.bit_depth - 3 + 3;
        let mut prev = first;
        for &colour in rest {
            let delta = i32::from(colour) - i32::from(prev); // ≥ 1 (sorted distinct)
            self.sym.encode_literal((delta - 1) as u32, palette_bits);
            palette_bits = palette_delta_bits(self.bit_depth, colour, palette_bits);
            prev = colour;
        }
    }

    /// Signals the luma `color_index_map_y` (§5.11.49): the top-left index via `ns()`, then the
    /// remaining indices in anti-diagonal (wavefront) order, each as the position of its color in the
    /// neighbour-derived `ColorOrder`, coded under the size/context palette-color CDF.
    fn signal_palette_tokens(&mut self, index_map: &[u8], bw: usize, psize: usize) {
        self.encode_ns(index_map[0] as usize, psize);
        for i in 1..(2 * bw - 1) {
            let j_start = i.min(bw - 1);
            let j_end = i.saturating_sub(bw - 1);
            let mut j = j_start;
            loop {
                let (rr, cc) = (i - j, j);
                let (order, ctx) = palette_color_context(index_map, bw, rr, cc, psize);
                let actual = index_map[rr * bw + cc] as usize;
                let sym = order.iter().position(|&x| x == actual).unwrap_or(0);
                self.sym.encode(sym, self.cdfs.palette_color_y(psize, ctx));
                if j == j_end {
                    break;
                }
                j -= 1;
            }
        }
    }

    /// Reconstructs a palette block: luma is `palette[ColorMapY]`, chroma is the (flat) DC prediction.
    /// No residual is coded (the block is `skip = 1`), and the neighbour level/dc contexts are reset.
    fn recon_palette(&mut self, r: usize, c: usize, bw: usize, pal: &PaletteBlock) {
        let (sx, sy) = (c * 4, r * 4);
        for i in 0..bw {
            for j in 0..bw {
                let idx = pal.index_map[i * bw + j] as usize;
                self.recon[0][(sy + i) * self.geom[0].coded_w + (sx + j)] = pal.colors[idx];
            }
        }
        self.set_ctx(0, sx >> 2, sy >> 2, bw / 4, bw / 4, 0, 0);
        // Chroma is flat DC over the plane's **own** residual block, at the plane's own base — not
        // the luma extent at the luma position. A palette block is always at least 8x8, so it
        // always has chroma of its own (§5.11.5) — unless the frame is monochrome, which has no
        // chroma plane and therefore no chroma residual size either.
        let Some((cw, ch)) = plane_residual_size(bw, bw, 1, self.subsampling) else {
            return;
        };
        for plane in 1..self.num_planes {
            let g = self.geom[plane];
            let (cx, cy) = ((c >> g.ss_x) * 4, (r >> g.ss_y) * 4);
            let dc = clip_pixel(self.dc_pred(plane, cx, cy, cw, ch), self.bit_depth);
            for i in 0..ch {
                for j in 0..cw {
                    self.recon[plane][(cy + i) * g.coded_w + (cx + j)] = dc;
                }
            }
            self.set_ctx(plane, cx >> 2, cy >> 2, cw / 4, ch / 4, 0, 0);
        }
    }

    /// Source-luma range (max − min) over the `bw × bw` block at coded `(sx, sy)`. Used by the
    /// partition and transform-depth heuristics.
    fn luma_range(&self, sx: usize, sy: usize, w: usize, h: usize) -> i32 {
        let mut lo = i32::MAX;
        let mut hi = i32::MIN;
        for i in 0..h {
            for j in 0..w {
                let v = self.sample(0, sx + j, sy + i);
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        hi - lo
    }

    /// Decides whether to code a square `bw × bw` region (16×16 or 32×32, fully on-screen) as two
    /// rectangular halves: `Some(1)` PARTITION_HORZ (top/bottom `bw × bw/2`) or `Some(2)` PARTITION_VERT
    /// (left/right `bw/2 × bw`). Chosen when the whole block is textured (would otherwise split) but a
    /// single horizontal or vertical edge makes both halves smooth — so each half gets one mode/
    /// transform. A quality decision (every choice reconstructs bit-exactly); the threshold is a
    /// deterministic placeholder. `None` falls through to the NONE/SPLIT decision.
    fn decide_rect(&self, r: usize, c: usize, bw: usize) -> Option<usize> {
        // 16 and 32 under the full search, none under a pruned one. A list rather than the two
        // `bw != 16 && bw != 32` comparisons it replaced, which also retires that `&&`'s mutant.
        if !self.search.rect_sizes.contains(&bw) {
            return None;
        }
        let n4 = bw / 4;
        if r + n4 > self.mi_rows || c + n4 > self.mi_cols {
            return None; // a partial block crops cleanly as smaller squares
        }
        let (sx, sy, hp) = (c * 4, r * 4, bw / 2);
        if self.luma_range(sx, sy, bw, bw) <= 32 {
            return None; // smooth enough to keep whole (PARTITION_NONE)
        }
        let horz_ok =
            self.luma_range(sx, sy, bw, hp) <= 32 && self.luma_range(sx, sy + hp, bw, hp) <= 32;
        let vert_ok =
            self.luma_range(sx, sy, hp, bw) <= 32 && self.luma_range(sx + hp, sy, hp, bw) <= 32;
        // §6.10.4: a partition whose chroma residual would be `BLOCK_INVALID` is not conformant.
        // Under 4:2:2 that rules out every taller-than-wide block — halving an 8x16 block's width
        // implies a 4x16 chroma block, an aspect ratio AV1 does not code — so `PARTITION_VERT` is
        // unavailable at every size. Derived from the table rather than special-cased, so a future
        // layout with different invalid shapes needs no change here. This costs the 4:2:2 encoder
        // half its rectangular partition set, which is a real (documented) rate cost.
        let horz_ok = horz_ok && self.rect_partition_is_codable(bw, hp);
        let vert_ok = vert_ok && self.rect_partition_is_codable(hp, bw);
        if horz_ok {
            Some(1)
        } else if vert_ok {
            Some(2)
        } else {
            None
        }
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

    /// Picks the luma `tx_depth` (0..=`max_depth`, itself ≤ `MAX_TX_DEPTH = 2`) for a lossy ≥8×8
    /// block: smoother blocks keep one block-size transform, more textured ones split the transform
    /// (one prediction mode, several sub-transforms). This is a quality decision — every depth
    /// reconstructs bit-exactly — so the range thresholds are a deterministic placeholder.
    fn select_tx_depth(&self, sx: usize, sy: usize, bw: usize, max_depth: usize) -> usize {
        let range = self.luma_range(sx, sy, bw, bw);
        let depth = if range > 24 {
            2
        } else if range > 12 {
            1
        } else {
            0
        };
        depth.min(max_depth)
    }

    fn encode_block(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
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
        // predicted id with no bits.
        let segid = if self.qindex > 0 {
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
        if self.qindex > 0 {
            self.apply_seg_quant(segid);
        }
        // The luma transform size is decided before the mode, so each mode candidate is priced
        // over the transforms it will actually be coded with; it is signalled after the mode info.
        // A rectangular block codes one transform that fills it (tx_depth = 0).
        let tx_depth = if lossy_large && !is_rect {
            let max_depth = if bw == 8 { 1 } else { 2 };
            self.select_tx_depth(c * 4, r * 4, bw, max_depth)
        } else {
            0
        };
        let luma_tx = if is_rect {
            max_tx_size_rect(bw, bh)
        } else if lossy_large {
            square_tx(bw >> tx_depth)
        } else {
            TxSize::Tx4x4
        };
        let luma = if palette.is_some() || self.qindex == 0 {
            // Palette requires YMode == DC_PRED (and no filter-intra); lossless also forces DC_PRED.
            DC_LUMA
        } else {
            self.select_luma_mode(r, c, bw, bh, luma_tx)
        };
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
        // uv_mode. Its CDF is indexed by the luma mode. `is_cfl_allowed` is `MiSize == BLOCK_4X4`
        // when Lossless, else `Max(w, h) <= 32` — so CfL is allowed for the lossy 4×4 and 8×8 blocks
        // and the lossless 4×4 blocks. When allowed, the encoder picks chroma-from-luma if it beats
        // plain DC_PRED, then emits read_cfl_alphas; otherwise uv_mode = DC_PRED.
        // §5.11.5 `HasChroma`: with subsampled chroma a sub-8x8 block can cover no chroma samples
        // of its own, and the covering block of its MI group codes chroma for the whole group. Such
        // a block codes no `uv_mode`, no `cfl_alpha` (§5.11.7) and no chroma residual (§5.11.34).
        // Always true at 4:4:4, and always false in a monochrome frame — the spec writes the
        // two parity tests first and then falls through to `HasChroma = NumPlanes > 1`.
        let block_has_chroma = has_chroma(r, c, bw, bh, self.subsampling);
        // §6.10.4 is a bitstream-conformance requirement the *partition search* has to respect, so
        // assert it at the point a block is committed rather than discovering it in the residual
        // loop. `decide_rect` is the only thing that can produce a shape that violates it.
        debug_assert!(
            !block_has_chroma || self.rect_partition_is_codable(bw, bh),
            "{bw}x{bh} has no codable chroma residual under {:?}",
            self.subsampling
        );
        let cfl_allowed = if self.qindex == 0 {
            bw == 4
        } else {
            bw.max(bh) <= 32
        };
        let is_lossy = self.qindex != 0;
        let cfl = match (block_has_chroma, palette.is_some()) {
            // No chroma of its own (or none in the frame at all), or a palette block — whose
            // chroma is plain DC_PRED.
            (false, _) | (_, true) => None,
            _ if is_lossy && cfl_allowed && !is_rect => self.select_cfl(r, c, bw, luma, luma_tx),
            // CfL is square-only here; a rectangular block's chroma is plain DC_PRED.
            _ => None,
        };
        let ym = usize::from(y_mode);
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
                let bctx = bw.trailing_zeros() as usize + bh.trailing_zeros() as usize - 6; // Mi_W_Log2 + Mi_H_Log2 - 2
                let above = r > self.tile_r0 && self.mi_psize[(r - 1) * self.mi_cols + c] > 0;
                let left = c > self.tile_c0 && self.mi_psize[r * self.mi_cols + (c - 1)] > 0;
                let pctx = usize::from(above) + usize::from(left);
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
        if is_rect {
            // One rectangular transform fills the block (tx_depth = 0). The CDF is keyed by the
            // bounding square (`txSzSqrUp = Max(bw, bh)`); the context is `(aboveTxW ≥ bw) +
            // (leftTxH ≥ bh)`, exactly as for a square block of that size.
            let ctx = self.tx_depth_ctx(r, c, bw, bh);
            let slot = if bw.max(bh) == 16 {
                self.cdfs.tx_size_16x16[ctx].slot()
            } else {
                self.cdfs.tx_size_32x32[ctx].slot()
            };
            self.sym.encode(0, slot);
        } else if lossy_large {
            let ctx = self.tx_depth_ctx(r, c, bw, bw);
            let slot = match bw {
                8 => self.cdfs.tx_size_8x8[ctx].slot(),
                16 => self.cdfs.tx_size_16x16[ctx].slot(),
                32 => self.cdfs.tx_size_32x32[ctx].slot(),
                _ => self.cdfs.tx_size_64x64[ctx].slot(),
            };
            self.sym.encode(tx_depth, slot);
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
            for ty in 0..bw / 4 {
                for tx in 0..bw / 4 {
                    let by = (r + ty) as isize - self.sb_r as isize;
                    let bx = (c + tx) as isize - self.sb_c as isize;
                    if (0..16).contains(&by) && (0..16).contains(&bx) {
                        self.block_decoded[(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] = 1;
                    }
                }
            }
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

    /// The transform a `bw × bh` block's chroma residual is coded with: the block-size transform
    /// on the lossy path (capped at 32, §5.11.37), 4×4 below 8×8 and on the lossless path.
    fn chroma_tx(&self, bw: usize, bh: usize) -> TxSize {
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
    fn code_plane(
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
            // §6.10.4 forbids and the partition search must never emit; coding one would
            // produce a stream both reference decoders reject.
            panic!("no plane-{plane} residual size for a {bw}x{bh} block");
        };
        let (pw, ph) = (ptx.width(), ptx.height());
        // §5.11.34 `baseXBlock`/`baseYBlock`: the MI position is scaled into the plane *before*
        // it is converted to samples, so an odd MI column lands on the covering chroma block.
        let base_x = (c >> g.ss_x) * 4;
        let base_y = (r >> g.ss_y) * 4;
        let (mut sse, mut rate) = (0, 0);
        let mut sy = base_y;
        while sy < base_y + rh {
            let mut sx = base_x;
            while sx < base_x + rw {
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
                    for ty in 0..ph / 4 {
                        for tx in 0..pw / 4 {
                            let by = (sy / 4 + ty) as isize - self.sb_r as isize;
                            let bx = (sx / 4 + tx) as isize - self.sb_c as isize;
                            if (0..16).contains(&by) && (0..16).contains(&bx) {
                                self.block_decoded
                                    [(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] = 1;
                            }
                        }
                    }
                }
                sx += pw;
            }
            sy += ph;
        }
        (sse, rate)
    }

    /// Trials `plane`'s residual for the block at MI `(r, c)` under `pred`, then puts back
    /// everything the trial touched — the plane's reconstruction over the block, its coefficient
    /// contexts, `BlockDecoded` and the CfL luma bounds — so the encoder is left exactly as it was.
    /// Returns the `(SSE, rate)` the trial measured.
    #[allow(clippy::too_many_arguments)]
    fn trial_plane(
        &mut self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        ptx: TxSize,
        pred: Pred,
    ) -> (u64, u32) {
        let saved = self.save_plane(plane, r, c, bw, bh);
        let measured = self.code_plane(plane, r, c, bw, bh, ptx, pred, false, Coding::Trial);
        self.restore_plane(saved);
        measured
    }

    /// Captures what coding `plane`'s residual for the block at MI `(r, c)` can change.
    fn save_plane(&self, plane: usize, r: usize, c: usize, bw: usize, bh: usize) -> PlaneState {
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
        PlaneState {
            plane,
            x0,
            y0,
            w,
            recon,
            above_level: self.above_level[plane].clone(),
            above_dc: self.above_dc[plane].clone(),
            left_level: self.left_level[plane].clone(),
            left_dc: self.left_dc[plane].clone(),
            block_decoded: self.block_decoded.clone(),
            max_luma: (self.max_luma_w, self.max_luma_h),
        }
    }

    /// The squared error of the current reconstruction over the region `s` captured.
    fn region_sse(&self, s: &PlaneState) -> u64 {
        let stride = self.geom[s.plane].coded_w;
        let mut region = Vec::with_capacity(s.recon.len());
        for y in s.y0..s.y0 + s.recon.len().checked_div(s.w).unwrap_or(0) {
            let at = y * stride + s.x0;
            region.extend_from_slice(&self.recon[s.plane][at..at + s.w]);
        }
        self.sse(s.plane, s.x0, s.y0, s.w.max(1), &region)
    }

    /// Puts back what [`Self::save_plane`] captured.
    fn restore_plane(&mut self, s: PlaneState) {
        let p = s.plane;
        if s.w > 0 {
            self.store_recon(p, s.x0, s.y0, s.w, &s.recon);
        }
        self.above_level[p] = s.above_level;
        self.above_dc[p] = s.above_dc;
        self.left_level[p] = s.left_level;
        self.left_dc[p] = s.left_dc;
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
        if self.qindex == 0 {
            let pred = self.predict_4x4(plane, sx, sy, desc.mode);
            let mut res = [0i32; 16];
            for i in 0..4 {
                for j in 0..4 {
                    res[i * 4 + j] = self.sample(plane, sx + j, sy + i) - pred[i * 4 + j];
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
        let pred: Vec<i32> = match desc.filter_intra {
            Some(fi) if plane == 0 => self.predict_filter_intra(plane, sx, sy, tw, fi),
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
        let choice = self.select_tx_type(&site, &pred, &res);
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
            Coding::Trial => {
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
        if tw == 64 && th == 64 {
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
    fn rd_cost(&self, sse: u64, rate: u32) -> u64 {
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
    /// `TX_SET_INTRA_2`. A 32×32 or larger luma block is `TX_SET_DCTONLY` (§5.11.48), and chroma and
    /// the rectangular transforms are coded `DCT_DCT` here, so they price that one candidate. The
    /// choice is a quality decision (any valid type round-trips bit-exactly); only the *signaling*
    /// must be correct.
    fn select_tx_type(&self, site: &TxSite, pred: &[i32], res: &[i32]) -> TxChoice {
        let (tw, th) = (site.tx_size.width(), site.tx_size.height());
        let trials = if site.plane == 0 && tw == th && tw < 32 {
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
        // DCT_DCT (symbol 1) first, so it wins ties; the rest of TX_SET_INTRA_2 follows.
        let mut best = price(TxType::DctDct, 1);
        for &(tx, symbol) in trials {
            let candidate = price(tx, symbol);
            if candidate.0 < best.0 {
                best = candidate;
            }
        }
        best.1
    }

    /// DC intra prediction value for a 4×4 at coded position `(sx, sy)` (§7.11.2.5). In the lossless
    /// path neighbours are the (padded) source (which equals the reconstruction); in the lossy path
    /// they are the reconstruction buffer, matching the decoder exactly.
    fn dc_avg(&self, plane: usize, sx: usize, sy: usize) -> i32 {
        self.dc_pred(plane, sx, sy, 4, 4)
    }

    /// Whether a `bw` x `bh` block has a codable chroma residual under this frame's subsampling
    /// (§5.11.38 / §6.10.4). Always true at 4:4:4 and 4:2:0, and in a monochrome frame — which has
    /// no chroma plane to constrain; false for taller-than-wide blocks at 4:2:2.
    fn rect_partition_is_codable(&self, bw: usize, bh: usize) -> bool {
        !self.has_chroma() || plane_residual_size(bw, bh, 1, self.subsampling).is_some()
    }

    /// The tile's left edge in `plane`'s own sample coordinates.
    ///
    /// `tile_x0` is a **luma** position, and prediction availability is tested against a
    /// plane-local one. Tiles start on superblock boundaries (multiples of 16 MI cells, so 64 luma
    /// samples), which makes the shift exact. Identity at 4:4:4 — and invisible there, which is why
    /// a single-tile frame and a frame whose second tile is one chroma block wide both pass
    /// regardless.
    fn tile_left(&self, plane: usize) -> usize {
        self.tile_x0 >> self.geom[plane].ss_x
    }

    /// The tile's top edge in `plane`'s own sample coordinates: [`Self::tile_left`] for rows.
    /// Tile rows also start on superblock boundaries, so the shift is exact here too.
    fn tile_top(&self, plane: usize) -> usize {
        self.tile_y0 >> self.geom[plane].ss_y
    }

    /// The plane-local base position and residual extent of the block at MI `(r, c)` — the pair a
    /// per-plane loop needs in order to read `plane`'s own samples rather than the luma ones.
    /// `None` is §5.11.38's `BLOCK_INVALID`.
    fn plane_block(
        &self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        plane: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        let g = self.geom[plane];
        let (rw, rh) = plane_residual_size(bw, bh, plane, self.subsampling)?;
        Some(((c >> g.ss_x) * 4, (r >> g.ss_y) * 4, rw, rh))
    }

    /// `DC_PRED` value for an `n × n` block at coded `(sx, sy)` (§7.11.2.5): the rounded average of
    /// the available `n` above and `n` left neighbours (both sides ⇒ `Round2(sum, log2(n) + 1)`; one
    /// side ⇒ `Round2(sum, log2(n))`; neither ⇒ `1 << (BitDepth - 1)`).
    fn dc_pred(&self, plane: usize, sx: usize, sy: usize, w: usize, h: usize) -> i32 {
        let nb = |x: usize, y: usize| -> i32 {
            if self.qindex > 0 {
                i32::from(self.recon[plane][y * self.geom[plane].coded_w + x])
            } else {
                self.sample(plane, x, y)
            }
        };
        let have_above = sy > self.tile_top(plane);
        let have_left = sx > self.tile_left(plane);
        // §7.11.2.5 DC: average the `w` above + `h` left samples (a plain integer divide, since for a
        // rectangular block `w + h` is not a power of two; for square blocks this is the usual shift).
        match (have_above, have_left) {
            (true, true) => {
                let mut s = 0;
                for k in 0..w {
                    s += nb(sx + k, sy - 1);
                }
                for k in 0..h {
                    s += nb(sx - 1, sy + k);
                }
                (s + ((w + h) as i32 >> 1)) / (w + h) as i32
            }
            (false, true) => {
                let mut s = 0;
                for k in 0..h {
                    s += nb(sx - 1, sy + k);
                }
                (s + (h as i32 >> 1)) / h as i32
            }
            (true, false) => {
                let mut s = 0;
                for k in 0..w {
                    s += nb(sx + k, sy - 1);
                }
                (s + (w as i32 >> 1)) / w as i32
            }
            (false, false) => self.neutral(),
        }
    }

    /// Builds the §7.11.2.1 reference samples for a 4×4 transform block at coded `(sx, sy)`:
    /// `AboveRow[0..8]` and `LeftCol[0..8]` (the `w + h` samples), plus the top-left corner
    /// (`AboveRow[-1] == LeftCol[-1]`). Reads the reconstruction buffer with the spec's availability
    /// fallbacks: when a neighbour is missing the side defaults to `127`/`129` (or replicates the
    /// orthogonal edge); samples past the block width replicate the last valid one unless the
    /// above-right / below-left 4×4 has been decoded (`BlockDecoded`, §5.11.34), in which case they
    /// are real samples.
    fn reference_4x4(&self, plane: usize, sx: usize, sy: usize) -> ([i32; 8], [i32; 8], i32) {
        let nb = |x: usize, y: usize| -> i32 {
            i32::from(self.recon[plane][y * self.geom[plane].coded_w + x])
        };
        let have_above = sy > self.tile_top(plane);
        let have_left = sx > self.tile_left(plane);

        // Above-right / below-left availability (§5.11.34) for the directional zone-1/3 angles.
        let (by, bx) = (
            sy as isize / 4 - self.sb_r as isize,
            sx as isize / 4 - self.sb_c as isize,
        );
        let have_above_right = self.block_decoded_at(by - 1, bx + 1);
        let have_below_left = self.block_decoded_at(by + 1, bx - 1);
        let (max_x, max_y) = (self.geom[plane].coded_w - 1, self.geom[plane].coded_h - 1);

        let mut above = [self.neutral() - 1; 8]; // when neither neighbour exists
        if have_above {
            let above_limit = max_x.min(sx + if have_above_right { 8 } else { 4 } - 1);
            for (i, a) in above.iter_mut().enumerate() {
                *a = nb(above_limit.min(sx + i), sy - 1);
            }
        } else if have_left {
            above = [nb(sx - 1, sy); 8]; // CurrFrame[y][x-1]
        }

        let mut left = [self.neutral() + 1; 8]; // when neither neighbour exists
        if have_left {
            let left_limit = max_y.min(sy + if have_below_left { 8 } else { 4 } - 1);
            for (i, l) in left.iter_mut().enumerate() {
                *l = nb(sx - 1, left_limit.min(sy + i));
            }
        } else if have_above {
            left = [nb(sx, sy - 1); 8]; // CurrFrame[y-1][x]
        }

        let top_left = if have_above && have_left {
            nb(sx - 1, sy - 1)
        } else if have_above {
            nb(sx, sy - 1)
        } else if have_left {
            nb(sx - 1, sy)
        } else {
            self.neutral()
        };
        (above, left, top_left)
    }

    /// Builds the `n × n` recursive filter-intra prediction (§7.11.2.3) for luma at coded `(sx, sy)`
    /// under `filter_intra_mode`. The block is tiled into `w4 = n/4` by `h2 = n/2` overlapping 4×2
    /// sub-blocks, processed top-to-bottom then left-to-right so each sub-block can filter the already
    /// predicted samples of the one above and to its left. Each output 4×2 weights a 7-sample
    /// neighbourhood `p` by [`cdf::INTRA_FILTER_TAPS`] and scales by `INTRA_FILTER_SCALE_BITS = 4`
    /// (`Round2Signed`). Only `AboveRow[-1..n-1]` / `LeftCol[-1..n-1]` are read, so no above-right /
    /// below-left extension is needed. Used only on the lossy path, where `enable_filter_intra = 1`.
    fn predict_filter_intra(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        n: usize,
        fi_mode: u8,
    ) -> Vec<i32> {
        let (above, left, top_left) = self.reference_basic(plane, sx, sy, n, n);
        let above_row = |k: i32| -> i32 { if k < 0 { top_left } else { above[k as usize] } };
        let left_col = |k: i32| -> i32 { if k < 0 { top_left } else { left[k as usize] } };
        let taps = &cdf::INTRA_FILTER_TAPS[fi_mode as usize];
        let (w4, h2, nn) = ((n >> 2) as i32, (n >> 1) as i32, n as i32);
        let mut pred = vec![0i32; n * n];
        // For each 4×2 sub-block (i2 = rows of 2, j4 = columns of 4), in this exact order so the
        // dependencies on the sub-block above (`(i2<<1)-1`) and to the left (`(j4<<2)-1`) are ready.
        for i2 in 0..h2 {
            for j4 in 0..w4 {
                // p[0..6]: the 7 neighbouring samples (§7.11.2.3). Top row reads AboveRow; left column
                // reads LeftCol; the interior reads already-predicted samples.
                let mut p = [0i32; 7];
                for (i, pi) in p.iter_mut().enumerate() {
                    let i = i as i32;
                    *pi = if i < 5 {
                        if i2 == 0 {
                            above_row((j4 << 2) + i - 1)
                        } else if j4 == 0 && i == 0 {
                            left_col((i2 << 1) - 1)
                        } else {
                            pred[(((i2 << 1) - 1) * nn + (j4 << 2) + i - 1) as usize]
                        }
                    } else if j4 == 0 {
                        left_col((i2 << 1) + i - 5)
                    } else {
                        pred[(((i2 << 1) + i - 5) * nn + (j4 << 2) - 1) as usize]
                    };
                }
                for i1 in 0..2i32 {
                    for j1 in 0..4i32 {
                        let row = ((i1 << 2) + j1) as usize;
                        let mut pr = 0i32;
                        for (k, &tap) in taps[row].iter().enumerate() {
                            pr += i32::from(tap) * p[k];
                        }
                        let idx = (((i2 << 1) + i1) * nn + (j4 << 2) + j1) as usize;
                        pred[idx] = i32::from(clip_pixel(round2_signed(pr, 4), self.bit_depth));
                    }
                }
            }
        }
        pred
    }

    /// Builds the 4×4 intra prediction for `plane` at coded position `(sx, sy)` under `mode`
    /// (§7.11.2). `DC_PRED` replicates [`Self::dc_avg`]; the other supported modes (non-directional
    /// `PAETH`/`SMOOTH`/`SMOOTH_V`/`SMOOTH_H` and directional `V`/`H`/`D135`/`D113`/`D157`) are only
    /// used for lossy luma and read the reconstruction buffer, applying the spec's reference-sample
    /// availability fallbacks. Returns the prediction row-major. With `enable_intra_edge_filter = 0`
    /// and `angle_delta = 0` (4×4), the supported modes never read past the 4 above / 4 left samples
    /// plus the top-left corner, so above-right / below-left extension is not needed.
    fn predict_4x4(&self, plane: usize, sx: usize, sy: usize, mode: u8) -> [i32; 16] {
        if mode == DC_PRED {
            return [self.dc_avg(plane, sx, sy); 16];
        }
        let (above, left, top_left) = self.reference_4x4(plane, sx, sy);

        // Directional derivatives (`Dr_Intra_Derivative`). Zone 1 (pAngle < 90): dx = Dr[pAngle].
        // Zone 2 (90 < pAngle < 180): dx = Dr[180 - pAngle], dy = Dr[pAngle - 90]. Zone 3
        // (pAngle > 180): dy = Dr[270 - pAngle]. Unused by the non-directional / cardinal modes.
        let (dx, dy): (i32, i32) = match mode {
            D45_PRED => (64, 0),    // Dr[45]
            D67_PRED => (27, 0),    // Dr[67]
            D135_PRED => (64, 64),  // Dr[45], Dr[45]
            D113_PRED => (27, 151), // Dr[67], Dr[23]
            D157_PRED => (151, 27), // Dr[23], Dr[67]
            D203_PRED => (0, 27),   // Dr[67]
            _ => (0, 0),
        };

        let w = cdf::SM_WEIGHTS_4X4;
        let mut pred = [0i32; 16];
        for i in 0..4 {
            for j in 0..4 {
                pred[i * 4 + j] = match mode {
                    V_PRED => above[j],
                    H_PRED => left[i],
                    D45_PRED | D67_PRED => {
                        // §7.11.2.4 zone 1 (pAngle < 90): interpolate along the (extended) above row.
                        let (ii, jj) = (i as i32, j as i32);
                        let idx = (ii + 1) * dx;
                        let base = (idx >> 6) + jj;
                        let max_base_x = 4 + 4 - 1; // (w + h - 1)
                        if base < max_base_x {
                            let shift = (idx >> 1) & 0x1F;
                            (above[base as usize] * (32 - shift)
                                + above[(base + 1) as usize] * shift
                                + 16)
                                >> 5
                        } else {
                            above[max_base_x as usize]
                        }
                    }
                    D203_PRED => {
                        // §7.11.2.4 zone 3 (pAngle > 180): interpolate along the (extended) left col.
                        let (ii, jj) = (i as i32, j as i32);
                        let idx = (jj + 1) * dy;
                        let base = (idx >> 6) + ii;
                        let shift = (idx >> 1) & 0x1F;
                        (left[base as usize] * (32 - shift)
                            + left[(base + 1) as usize] * shift
                            + 16)
                            >> 5
                    }
                    D135_PRED | D113_PRED | D157_PRED => {
                        // §7.11.2.4 zone 2 (upsample disabled): interpolate along the above row,
                        // falling back to the left column when the ray leaves the top edge. Indices
                        // -1 map to the top-left corner; the chosen angles keep them within [-1, 3].
                        let (ii, jj) = (i as i32, j as i32);
                        let idx = (jj << 6) - (ii + 1) * dx;
                        let base = idx >> 6;
                        if base >= -1 {
                            let shift = (idx >> 1) & 0x1F;
                            let a0 = if base < 0 {
                                top_left
                            } else {
                                above[base as usize]
                            };
                            let a1 = above[(base + 1) as usize];
                            (a0 * (32 - shift) + a1 * shift + 16) >> 5 // Round2(_, 5)
                        } else {
                            let idx2 = (ii << 6) - (jj + 1) * dy;
                            let base2 = idx2 >> 6;
                            let shift = (idx2 >> 1) & 0x1F;
                            let l0 = if base2 < 0 {
                                top_left
                            } else {
                                left[base2 as usize]
                            };
                            let l1 = left[(base2 + 1) as usize];
                            (l0 * (32 - shift) + l1 * shift + 16) >> 5 // Round2(_, 5)
                        }
                    }
                    PAETH_PRED => {
                        let base = above[j] + left[i] - top_left;
                        let p_left = (base - left[i]).abs();
                        let p_top = (base - above[j]).abs();
                        let p_tl = (base - top_left).abs();
                        if p_left <= p_top && p_left <= p_tl {
                            left[i]
                        } else if p_top <= p_tl {
                            above[j]
                        } else {
                            top_left
                        }
                    }
                    SMOOTH_PRED => {
                        let v = w[i] * above[j]
                            + (256 - w[i]) * left[3]
                            + w[j] * left[i]
                            + (256 - w[j]) * above[3];
                        (v + 256) >> 9 // Round2(_, 9)
                    }
                    SMOOTH_V_PRED => {
                        let v = w[i] * above[j] + (256 - w[i]) * left[3];
                        (v + 128) >> 8 // Round2(_, 8)
                    }
                    _ => {
                        // SMOOTH_H_PRED
                        let v = w[j] * left[i] + (256 - w[j]) * above[3];
                        (v + 128) >> 8 // Round2(_, 8)
                    }
                };
            }
        }
        pred
    }

    /// Builds the `n × n` intra prediction for `plane` at coded `(sx, sy)` under `mode`. 4×4 blocks
    /// dispatch to [`Self::predict_4x4`] (`angle_delta` is always 0 there — never signaled for
    /// `MiSize < BLOCK_8X8`). 8×8 and 16×16 blocks use the non-directional predictor for
    /// `DC`/`SMOOTH*`/`PAETH` and the general directional process (§7.11.2.4) — fine-tuned by
    /// `angle_delta` — otherwise.
    #[allow(clippy::too_many_arguments)]
    fn predict_intra(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        mode: u8,
        w: usize,
        h: usize,
        angle_delta: i8,
    ) -> Vec<i32> {
        // Directional prediction and the 4×4 fast path are square-only; a rectangular transform block
        // only ever uses the non-directional modes (the encoder never selects a directional mode for a
        // rectangular partition, so `angle_delta` is irrelevant there).
        if w != h {
            return self.predict_nondir(plane, sx, sy, w, h, mode);
        }
        let n = w;
        // Directional modes go through the general process so the block's `angle_delta` is honored —
        // including a 4×4 sub-transform of a ≥8×8 directional block under TX_MODE_SELECT (where
        // `angle_delta` may be non-zero). For `n == 4, angle_delta == 0` this matches `predict_4x4`
        // exactly (pinned by `general_directional_matches_4x4_path`).
        if is_directional(mode) {
            return self.predict_directional(plane, sx, sy, n, mode, angle_delta);
        }
        if n == 4 {
            return self.predict_4x4(plane, sx, sy, mode).to_vec();
        }
        self.predict_nondir(plane, sx, sy, n, n, mode)
    }

    /// Builds the §7.11.2.2 reference samples for an `n × n` directional block: `AboveRow[0..2n]`,
    /// `LeftCol[0..2n]`, and the top-left corner (`AboveRow[-1] == LeftCol[-1]`). The extended
    /// (`> n`) samples are real reconstruction only when the above-right / below-left `n × n` region
    /// has been decoded (`BlockDecoded`, §5.11.34, stepped by `n / 4` MI cells); otherwise they
    /// replicate the last in-block edge sample. With `enable_intra_edge_filter = 0` no edge filtering
    /// or upsampling is applied. Generalizes [`Self::reference_4x4`] (which is the `n == 4` case).
    fn reference_directional(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        n: usize,
    ) -> (Vec<i32>, Vec<i32>, i32) {
        let nb = |x: usize, y: usize| -> i32 {
            i32::from(self.recon[plane][y * self.geom[plane].coded_w + x])
        };
        let have_above = sy > self.tile_top(plane);
        let have_left = sx > self.tile_left(plane);
        let step = (n / 4) as isize;
        let (by, bx) = (
            sy as isize / 4 - self.sb_r as isize,
            sx as isize / 4 - self.sb_c as isize,
        );
        let have_above_right = self.block_decoded_at(by - 1, bx + step);
        let have_below_left = self.block_decoded_at(by + step, bx - 1);
        let (max_x, max_y) = (self.geom[plane].coded_w - 1, self.geom[plane].coded_h - 1);

        let mut above = vec![self.neutral() - 1; 2 * n];
        if have_above {
            let above_limit = max_x.min(sx + if have_above_right { 2 * n } else { n } - 1);
            for (i, a) in above.iter_mut().enumerate() {
                *a = nb(above_limit.min(sx + i), sy - 1);
            }
        } else if have_left {
            above = vec![nb(sx - 1, sy); 2 * n];
        }

        let mut left = vec![self.neutral() + 1; 2 * n];
        if have_left {
            let left_limit = max_y.min(sy + if have_below_left { 2 * n } else { n } - 1);
            for (i, l) in left.iter_mut().enumerate() {
                *l = nb(sx - 1, left_limit.min(sy + i));
            }
        } else if have_above {
            left = vec![nb(sx, sy - 1); 2 * n];
        }

        let top_left = if have_above && have_left {
            nb(sx - 1, sy - 1)
        } else if have_above {
            nb(sx, sy - 1)
        } else if have_left {
            nb(sx - 1, sy)
        } else {
            self.neutral()
        };
        (above, left, top_left)
    }

    /// Directional intra prediction (§7.11.2.4) for an `n × n` block, with `pAngle = Mode_To_Angle +
    /// angle_delta * ANGLE_STEP` and `upsampleAbove = upsampleLeft = 0` (the intra edge filter is
    /// disabled). Cardinal angles (`90`/`180`) copy a single edge; zone 1 (`< 90`) interpolates along
    /// the extended above row; zone 2 (`90 < · < 180`) interpolates along the above row, falling back
    /// to the left column past the top edge; zone 3 (`> 180`) interpolates along the extended left
    /// column. `Dr_Intra_Derivative` supplies `dx`/`dy`; each interpolation is `Round2(_, 5)`.
    fn predict_directional(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        n: usize,
        mode: u8,
        angle_delta: i8,
    ) -> Vec<i32> {
        let p_angle = cdf::MODE_TO_ANGLE[mode as usize] + i32::from(angle_delta) * 3;
        let (above, left, top_left) = self.reference_directional(plane, sx, sy, n);
        let above_row = |k: i32| -> i32 { if k < 0 { top_left } else { above[k as usize] } };
        let left_col = |k: i32| -> i32 { if k < 0 { top_left } else { left[k as usize] } };
        let mut pred = vec![0i32; n * n];
        for i in 0..n as i32 {
            for j in 0..n as i32 {
                let v = if p_angle == 90 {
                    above[j as usize]
                } else if p_angle == 180 {
                    left[i as usize]
                } else if p_angle < 90 {
                    // Zone 1: ray sweeps the above row (extended to the above-right).
                    let dx = cdf::DR_INTRA_DERIVATIVE[p_angle as usize];
                    let idx = (i + 1) * dx;
                    let base = (idx >> 6) + j;
                    let max_base_x = (2 * n - 1) as i32; // (w + h - 1)
                    if base < max_base_x {
                        let shift = (idx >> 1) & 0x1F;
                        (above[base as usize] * (32 - shift)
                            + above[(base + 1) as usize] * shift
                            + 16)
                            >> 5
                    } else {
                        above[max_base_x as usize]
                    }
                } else if p_angle < 180 {
                    // Zone 2: ray sweeps the above row, falling back to the left column past the top.
                    let dx = cdf::DR_INTRA_DERIVATIVE[(180 - p_angle) as usize];
                    let dy = cdf::DR_INTRA_DERIVATIVE[(p_angle - 90) as usize];
                    let idx = (j << 6) - (i + 1) * dx;
                    let base = idx >> 6;
                    if base >= -1 {
                        let shift = (idx >> 1) & 0x1F;
                        (above_row(base) * (32 - shift) + above_row(base + 1) * shift + 16) >> 5
                    } else {
                        let idx2 = (i << 6) - (j + 1) * dy;
                        let base2 = idx2 >> 6;
                        let shift = (idx2 >> 1) & 0x1F;
                        (left_col(base2) * (32 - shift) + left_col(base2 + 1) * shift + 16) >> 5
                    }
                } else {
                    // Zone 3: ray sweeps the left column (extended to the below-left).
                    let dy = cdf::DR_INTRA_DERIVATIVE[(270 - p_angle) as usize];
                    let idx = (j + 1) * dy;
                    let base = (idx >> 6) + i;
                    let shift = (idx >> 1) & 0x1F;
                    (left[base as usize] * (32 - shift) + left[(base + 1) as usize] * shift + 16)
                        >> 5
                };
                pred[(i * n as i32 + j) as usize] = v;
            }
        }
        pred
    }

    /// Builds the §7.11.2.1 reference samples for a non-directional `n × n` block: `AboveRow[0..n]`,
    /// `LeftCol[0..n]`, and the top-left corner, with the availability fallbacks (`127`/`129`/edge
    /// replication). The supported non-directional modes never read above-right / below-left, so the
    /// references stop at `n` samples (no `BlockDecoded` extension).
    fn reference_basic(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        w: usize,
        h: usize,
    ) -> (Vec<i32>, Vec<i32>, i32) {
        let nb = |x: usize, y: usize| -> i32 {
            i32::from(self.recon[plane][y * self.geom[plane].coded_w + x])
        };
        let have_above = sy > self.tile_top(plane);
        let have_left = sx > self.tile_left(plane);
        let (max_x, max_y) = (self.geom[plane].coded_w - 1, self.geom[plane].coded_h - 1);

        let mut above = vec![self.neutral() - 1; w];
        if have_above {
            for (i, a) in above.iter_mut().enumerate() {
                *a = nb(max_x.min(sx + i), sy - 1);
            }
        } else if have_left {
            above = vec![nb(sx - 1, sy); w];
        }

        let mut left = vec![self.neutral() + 1; h];
        if have_left {
            for (i, l) in left.iter_mut().enumerate() {
                *l = nb(sx - 1, max_y.min(sy + i));
            }
        } else if have_above {
            left = vec![nb(sx, sy - 1); h];
        }

        let top_left = if have_above && have_left {
            nb(sx - 1, sy - 1)
        } else if have_above {
            nb(sx, sy - 1)
        } else if have_left {
            nb(sx - 1, sy)
        } else {
            self.neutral()
        };
        (above, left, top_left)
    }

    /// Non-directional `n × n` intra prediction (§7.11.2): `DC_PRED`, `PAETH_PRED`, and the three
    /// `SMOOTH` modes, using the `n`-wide smooth weights (`Sm_Weights_Tx_NxN`). Reads the
    /// reconstruction buffer. Used for the lossy 8×8 and 16×16 non-directional luma blocks.
    fn predict_nondir(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        w: usize,
        h: usize,
        mode: u8,
    ) -> Vec<i32> {
        if mode == DC_PRED {
            return vec![self.dc_pred(plane, sx, sy, w, h); w * h];
        }
        let (above, left, top_left) = self.reference_basic(plane, sx, sy, w, h);
        // SMOOTH weights are per axis: `sw` (horizontal, indexed by column) and `sh` (vertical, by
        // row). For a square block `sw == sh`; for a rectangular block they differ in length.
        let sw = sm_weights(w);
        let sh = sm_weights(h);
        let mut pred = vec![0i32; w * h];
        for i in 0..h {
            for j in 0..w {
                pred[i * w + j] = match mode {
                    PAETH_PRED => {
                        let base = above[j] + left[i] - top_left;
                        let p_left = (base - left[i]).abs();
                        let p_top = (base - above[j]).abs();
                        let p_tl = (base - top_left).abs();
                        if p_left <= p_top && p_left <= p_tl {
                            left[i]
                        } else if p_top <= p_tl {
                            above[j]
                        } else {
                            top_left
                        }
                    }
                    SMOOTH_PRED => {
                        let v = sh[i] * above[j]
                            + (256 - sh[i]) * left[h - 1]
                            + sw[j] * left[i]
                            + (256 - sw[j]) * above[w - 1];
                        (v + 256) >> 9
                    }
                    SMOOTH_V_PRED => {
                        let v = sh[i] * above[j] + (256 - sh[i]) * left[h - 1];
                        (v + 128) >> 8
                    }
                    _ => {
                        // SMOOTH_H_PRED
                        let v = sw[j] * left[i] + (256 - sw[j]) * above[w - 1];
                        (v + 128) >> 8
                    }
                };
            }
        }
        pred
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

    /// The rate, in [`RATE_ONE`] units, of signalling `cand` as the luma mode of the lossy
    /// `bw × bh` block at MI `(r, c)` (no palette): `y_mode`, then `angle_delta_y` (directional,
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
            let bctx = bw.trailing_zeros() as usize + bh.trailing_zeros() as usize - 6;
            let above = r > self.tile_r0 && self.mi_psize[(r - 1) * self.mi_cols + c] > 0;
            let left = c > self.tile_c0 && self.mi_psize[r * self.mi_cols + (c - 1)] > 0;
            let pctx = usize::from(above) + usize::from(left);
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

    /// Picks the luma prediction for the lossy `bw × bh` block at MI `(r, c)`, whose luma is coded
    /// with `luma_tx` transforms, by rate–distortion cost.
    ///
    /// Every candidate of [`Self::luma_candidates`] is trialled: its luma residual is coded
    /// transform by transform (each transform predicted from the reconstruction its predecessors
    /// left, under its own rate–distortion transform-type choice), and the reconstruction's squared
    /// error weighed against the coefficient rate plus the mode's signalling rate by
    /// [`Self::rd_cost`]. No candidate is ranked out by a cheaper proxy first, so a search that
    /// tries more candidates can only find a cheaper one. Earlier candidates win ties, so the
    /// cheaper signalling is preferred. The choice is a quality decision; every option
    /// reconstructs bit-exactly, so only the signaling must be correct.
    fn select_luma_mode(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        luma_tx: TxSize,
    ) -> LumaMode {
        let mut best: Option<(u64, LumaMode)> = None;
        for cand in self.luma_candidates(bw, bh) {
            let pred = Pred {
                mode: cand.mode,
                angle_delta: cand.angle_delta,
                filter_intra: cand.filter_intra,
                cfl_alpha: None,
            };
            let (sse, rate) = self.trial_plane(0, r, c, bw, bh, luma_tx, pred);
            let cost = self.rd_cost(sse, rate + self.luma_mode_rate(r, c, bw, bh, cand));
            if best.is_none_or(|(b, _)| cost < b) {
                best = Some((cost, cand));
            }
        }
        best.map_or(DC_LUMA, |(_, m)| m)
    }

    /// Adds the chroma-from-luma high-frequency term to a chroma DC prediction in place (§7.11.5).
    ///
    /// `L[i][j]` is the **sum** of the `(1 << subX) x (1 << subY)` reconstructed luma samples the
    /// chroma sample covers, shifted left by `3 - subX - subY`. That is exact at every subsampling —
    /// all three layouts land on the same 3-fractional-bit scale with no rounding — so it must not
    /// be written as "average, then scale", which truncates. At 4:4:4 it reduces to `luma << 3`.
    ///
    /// `lumaAvg = Round2(ΣL, Tx_Width_Log2 + Tx_Height_Log2)`, and each chroma sample becomes
    /// `Clip1(dc + Round2Signed(alpha * (L - lumaAvg), 6))`. `alpha == 0` is a no-op (plain DC).
    fn apply_cfl(&self, pred: &mut [i32], sx: usize, sy: usize, alpha: i32, tx_size: TxSize) {
        let (w, h) = (tx_size.width(), tx_size.height());
        let g = self.geom[1];
        let (sub_x, sub_y) = (g.ss_x as usize, g.ss_y as usize);
        let luma_stride = self.geom[0].coded_w;
        let mut l = vec![0i32; w * h];
        let mut sum = 0i32;
        for i in 0..h {
            // The clamps are §7.11.5's edge rule. They are inert for every block this encoder
            // emits — `encode_partition` force-splits anything overhanging the MI grid, so the
            // covered luma always lies inside the last coded transform — but a future
            // frame-spanning PARTITION_NONE would need them, and the decoder applies them.
            let luma_y = ((sy + i) << sub_y).min(self.max_luma_h - (1 << sub_y));
            for j in 0..w {
                let luma_x = ((sx + j) << sub_x).min(self.max_luma_w - (1 << sub_x));
                let mut t = 0i32;
                for dy in 0..=sub_y {
                    for dx in 0..=sub_x {
                        t += i32::from(self.recon[0][(luma_y + dy) * luma_stride + (luma_x + dx)]);
                    }
                }
                let v = t << (3 - sub_x - sub_y);
                l[i * w + j] = v;
                sum += v;
            }
        }
        let shift = tx_size.log2_width() + tx_size.log2_height();
        // The shared `Round2`, not an open-coded addend: the test transcribes §7.11.5 separately,
        // and two independent spellings of the same rounding is the point of that cross-check.
        let luma_avg = round2(i64::from(sum), shift) as i32;
        let max = (1i32 << self.bit_depth) - 1;
        for (p, &lv) in pred.iter_mut().zip(&l) {
            *p = (*p + round2_signed(alpha * (lv - luma_avg), 6)).clamp(0, max);
        }
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
                let (sse, rate) = self.trial_plane(plane, r, c, bw, bw, ctx, pred);
                out.push((alpha, sse, rate));
            }
        }
        self.restore_plane(luma_state);

        let uv = self.cdfs.uv_mode_cfl_allowed[usize::from(luma.mode)].probs();
        let mut dc_rate = symbol_rate(uv, usize::from(DC_PRED));
        if (8..=64).contains(&bw) {
            dc_rate += symbol_rate(self.cdfs.palette_uv_mode[0].probs(), 0);
        }
        let cfl_rate = symbol_rate(uv, usize::from(UV_CFL_PRED));
        let [u, v] = &trials;
        let (u0, v0) = (u[0], v[0]);
        let mut best = (self.rd_cost(u0.1 + v0.1, u0.2 + v0.2 + dc_rate), None);
        for &(au, su, ru) in u {
            for &(av, sv, rv) in v {
                if au == 0 && av == 0 {
                    continue;
                }
                let rate = ru + rv + cfl_rate + self.cfl_alphas_rate(au, av);
                let cost = self.rd_cost(su + sv, rate);
                if cost < best.0 {
                    best = (cost, Some((au, av)));
                }
            }
        }
        best.1
    }

    /// The rate of `read_cfl_alphas` (§5.11.45) for `(au, av)` against the CDFs as they stand,
    /// in [`RATE_ONE`] units: what [`Self::emit_cfl_alphas`] codes.
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

    /// Codes one transform block's coefficients (§5.11.39) and updates the neighbour level/DC
    /// contexts it leaves behind.
    #[allow(clippy::too_many_arguments)]
    fn code_coeffs(
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
    fn coeff_rate(
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

        let rect_scan;
        let scan: &[usize] = match (w, h) {
            (4, 4) => &cdf::DEFAULT_SCAN_4X4,
            (8, 8) => &cdf::DEFAULT_SCAN_8X8,
            (16, 16) => &cdf::DEFAULT_SCAN_16X16,
            (32, 32) | (64, 64) => &cdf::DEFAULT_SCAN_32X32, // both code a 32×32 region
            _ => {
                rect_scan = cdf::default_scan(code_w, code_h);
                &rect_scan
            }
        };

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
        sink.symbol(eobpt - 1, CoeffCdf::EobPt(eob_pt_class(area), ptype));
        if eobpt >= 3 {
            let nbits = eobpt - 2;
            let base_eob_val = (1usize << (eobpt - 2)) + 1;
            let extra = eob - base_eob_val;
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
                let neg = quant[pos] < 0;
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
        let dc_cat = if quant[0] == 0 {
            0
        } else if quant[0] < 0 {
            1
        } else {
            2
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
    fn set_ctx(
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

/// One bit of rate, in the fixed-point units every rate estimate is kept in (1/256 bit).
const RATE_ONE: u32 = 1 << 8;

/// `log2(x)` in [`RATE_ONE`] units, for `x >= 1`: the integer part from the bit length, then
/// eight fractional bits by repeated squaring of the normalised mantissa. Integer-only, so a rate
/// estimate — and therefore every decision it ranks — is identical on every target.
fn log2_rate(x: u32) -> u32 {
    let int = x.ilog2();
    // The mantissa `x / 2^int` in Q16, in `[1, 2)`.
    let mut m = (u64::from(x) << 16) >> int;
    let mut frac = 0;
    for bit in (0..8).rev() {
        m = (m * m) >> 16;
        if m >= 2 << 16 {
            m >>= 1;
            frac |= 1 << bit;
        }
    }
    int * RATE_ONE + frac
}

/// The rate of coding `symbol` against the cumulative `cdf` (§8.2.6 orientation: values rise to
/// 32768), in [`RATE_ONE`] units: `-log2(p)` for the symbol's probability `p / 32768`.
fn symbol_rate(cdf: &[u16], symbol: usize) -> u32 {
    let below = if symbol > 0 { cdf[symbol - 1] } else { 0 };
    let p = u32::from(cdf[symbol] - below).max(1);
    15 * RATE_ONE - log2_rate(p)
}

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

/// The `eob_pt_*` table (§5.11.39) for a transform coding `area` coefficients, as the index
/// [`CoeffCdf::EobPt`] carries: 16, 32, 64, 128, 256, 512 and 1024 map to 0..=6.
///
/// Deliberately not a catch-all: a transform area with no table of its own would be coded against
/// the wrong one and desynchronise the decoder, which surfaces only as a corrupt stream. Fail at
/// the encoder instead.
fn eob_pt_class(area: usize) -> usize {
    match area {
        16 | 32 | 64 | 128 | 256 | 512 | 1024 => area.ilog2() as usize - 4,
        other => panic!("no eob_pt CDF for a {other}-coefficient transform"),
    }
}

/// The tile's symbol coder. Normally every symbol goes into the range coder; while a decision
/// trials a candidate ([`FrameEncoder::trial_cost`]), `tally` is set and symbols are priced into
/// it instead — against the CDFs as they stand, which are then not adapted, so a trial leaves
/// both the coded stream and the CDFs untouched.
#[derive(Clone, Default)]
struct Coder {
    range: SymbolEncoder,
    /// The rate of the symbols trialled so far, in [`RATE_ONE`] units, or `None` when coding.
    tally: Option<u32>,
}

impl Coder {
    /// Codes `symbol` against the adapting CDF `slot` (or prices it, in a trial).
    fn encode(&mut self, symbol: usize, slot: cdf::CdfSlot<'_>) {
        match &mut self.tally {
            Some(rate) => *rate += symbol_rate(slot.probs(), symbol),
            None => cdf::encode(&mut self.range, symbol, slot),
        }
    }

    /// Codes `symbol` against a static CDF (or prices it, in a trial).
    fn encode_symbol(&mut self, symbol: usize, cdf: &[u16]) {
        match &mut self.tally {
            Some(rate) => *rate += symbol_rate(cdf, symbol),
            None => self.range.encode_symbol(symbol, cdf),
        }
    }

    /// Codes the low `n` bits of `value` as equiprobable bits (or prices them, in a trial).
    fn encode_literal(&mut self, value: u32, n: u32) {
        match &mut self.tally {
            Some(rate) => *rate += n * RATE_ONE,
            None => self.range.encode_literal(value, n),
        }
    }
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

/// The divisor of `q²` that gives the rate–distortion multiplier `λ` (see
/// [`FrameEncoder::rd_cost`]).
const LAMBDA_DIV: u64 = 1024;

/// The `CflSign` of a CfL alpha (§5.11.45): `CFL_SIGN_ZERO` (0), `CFL_SIGN_NEG` (1) or
/// `CFL_SIGN_POS` (2).
fn cfl_sign(alpha: i32) -> usize {
    if alpha == 0 {
        0
    } else if alpha < 0 {
        1
    } else {
        2
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

/// Plain `DC_PRED` luma: what palette and lossless blocks code.
const DC_LUMA: LumaMode = LumaMode {
    mode: DC_PRED,
    angle_delta: 0,
    filter_intra: None,
};

/// Whether a block is coded into the stream or only trialled: reconstructed and priced so a
/// decision can compare it against the alternatives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Coding {
    /// Code the symbols into the tile's range coder.
    Emit,
    /// Code nothing; reconstruct and price only.
    Trial,
}

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
struct PlaneState {
    plane: usize,
    /// The block's in-plane rectangle and its reconstruction before the trial.
    x0: usize,
    y0: usize,
    w: usize,
    recon: Vec<u16>,
    above_level: Vec<u8>,
    above_dc: Vec<u8>,
    left_level: Vec<u8>,
    left_dc: Vec<u8>,
    block_decoded: Vec<u8>,
    max_luma: (usize, usize),
}

/// What coding one block can change, captured by [`FrameEncoder::save_block`] so a trial can be
/// undone: the per-MI state over the block, the partition contexts, every plane's
/// [`PlaneState`], and the quantizer and loop-filter delta state a superblock's first block sets.
struct BlockState {
    /// The block's top-left MI cell and its width in MI cells within the frame.
    r: usize,
    c: usize,
    w4: usize,
    /// [`FrameEncoder::mi_maps`] over the block, row-major.
    maps: [Vec<u8>; 8],
    pcolors: Vec<[u16; 8]>,
    dlf: Vec<i8>,
    above_partition: Vec<u8>,
    left_partition: Vec<u8>,
    planes: Vec<PlaneState>,
    /// `(CurrentQIndex, CurrentDeltaLF, dc_quant, ac_quant, ReadDeltas)`.
    quant: (i32, i32, i32, i32, bool),
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

/// Derives the 2-symbol `split_or_horz` CDF from the partition CDF (§8.2.6): the vertical-ish
/// partition probabilities are folded into the "split" outcome.
fn split_or_horz_cdf(p: &[u16]) -> [u16; 2] {
    let psum = (p[2] - p[1])
        + (p[3] - p[2])
        + (p[4] - p[3])
        + (p[6] - p[5])
        + (p[7] - p[6])
        + (p[9] - p[8]);
    [32768 - psum, 32768]
}

/// Derives the 2-symbol `split_or_vert` CDF from the partition CDF (§8.3.2).
fn split_or_vert_cdf(p: &[u16]) -> [u16; 2] {
    let psum = (p[1] - p[0])
        + (p[3] - p[2])
        + (p[4] - p[3])
        + (p[5] - p[4])
        + (p[6] - p[5])
        + (p[8] - p[7]);
    [32768 - psum, 32768]
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

/// `NS(n)` non-symmetric literal (§4.10.7): codes a value in `0..n` with `floor(log2(n))` or +1 bits.
fn encode_ns(sym: &mut SymbolEncoder, v: u32, n: u32) {
    let w = (32 - n.leading_zeros()) as i32; // FloorLog2(n) + 1
    let m = (1u32 << w) - n;
    if v < m {
        sym.encode_literal(v, (w - 1) as u32);
    } else {
        let x = v + m;
        sym.encode_literal(x >> 1, (w - 1) as u32);
        sym.encode_literal(x & 1, 1);
    }
}

/// `v - mk`, the value coded once the sub-exponential prefix has narrowed the range to `[mk, …)`.
/// Shared between the two coding tails so it has a single mutation site: in the final-literal tail
/// `mk == 2^b2` and `encode_literal` keeps only the low `b2` bits, so there a `-`→`+` would be
/// invisible — but the `encode_ns` tail feeds it a raw value, where the same mutation is caught.
fn subexp_residual(v: u32, mk: u32) -> u32 {
    v - mk
}

/// `encode_subexp(v)` — the inverse of §4.10.8 `decode_subexp`: codes `v` in `0..num_syms` with a
/// sub-exponential prefix keyed by `k`.
fn encode_subexp(sym: &mut SymbolEncoder, v: u32, num_syms: u32, k: u32) {
    let (mut i, mut mk) = (0u32, 0u32);
    loop {
        let b2 = if i != 0 { k + i - 1 } else { k };
        let a = 1u32 << b2;
        if num_syms <= mk + 3 * a {
            encode_ns(sym, subexp_residual(v, mk), num_syms - mk);
            return;
        } else if v >= mk + a {
            sym.encode_literal(1, 1); // subexp_more_bits
            i += 1;
            mk += a;
        } else {
            sym.encode_literal(0, 1);
            sym.encode_literal(subexp_residual(v, mk), b2);
            return;
        }
    }
}

/// Recenters `v` around reference `r` (the inverse of §4.10.10 `inverse_recenter`).
fn recenter(r: i32, v: i32) -> u32 {
    if v > 2 * r {
        v as u32
    } else if v < r {
        (2 * (r - v) - 1) as u32
    } else {
        (2 * (v - r)) as u32
    }
}

/// `encode_unsigned_subexp_with_ref(v, mx, k, r)` (§4.10.9 inverse): codes `v` in `0..mx` against
/// reference `r` so that coding `v == r` emits the shortest (zero-delta) code.
fn encode_subexp_with_ref(sym: &mut SymbolEncoder, v: i32, mx: i32, k: u32, r: i32) {
    let recentered = if (r << 1) <= mx {
        recenter(r, v)
    } else {
        recenter(mx - 1 - r, mx - 1 - v)
    };
    encode_subexp(sym, recentered, mx as u32, k);
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

#[cfg(test)]
mod tests {
    use gamut_color::Planar8;

    use super::*;

    #[test]
    fn every_emitted_block_shape_has_a_codable_chroma_residual() {
        // §6.10.4: `get_plane_residual_size(subSize, 1)` must never be BLOCK_INVALID. Enumerates
        // the shapes `encode_partition` can produce — squares 4..64 and the PARTITION_HORZ/VERT
        // halves at 16 and 32 — against each layout the encoder codes.
        //
        // 4:2:2 is the constraint that bites: it rules out every taller-than-wide shape, which is
        // why `decide_rect` must not offer PARTITION_VERT there.
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

    #[test]
    fn the_partition_search_offers_no_vertical_split_at_four_two_two() {
        // Content shaped so a vertical split is the attractive one: two flat halves side by side
        // with a hard edge between them. At 4:4:4 and 4:2:0 `decide_rect` may take it; at 4:2:2 it
        // must not, because the resulting 8x16 block has no codable chroma residual.
        for ss in [
            ChromaSubsampling::Cs444,
            ChromaSubsampling::Cs420,
            ChromaSubsampling::Cs422,
        ] {
            let planes = planes_split_halves(ss);
            let e = FrameEncoder::new(&planes, 40);
            for (r, c) in [(0usize, 0usize), (0, 4), (4, 0), (4, 4)] {
                if let Some(kind) = e.decide_rect(r, c, 16) {
                    let (bw, bh) = if kind == 1 { (16, 8) } else { (8, 16) };
                    assert!(
                        plane_residual_size(bw, bh, 1, ss).is_some(),
                        "{ss:?}: decide_rect offered an uncodable {bw}x{bh}"
                    );
                }
            }
        }
    }

    /// A 32x32 image split into a dark left half and a bright right half, in `ss`.
    fn planes_split_halves(ss: ChromaSubsampling) -> Planar8 {
        let mut rgb = vec![0u8; 32 * 32 * 3];
        for y in 0..32usize {
            for x in 0..32usize {
                let v = if x < 16 { 20u8 } else { 200 };
                rgb[(y * 32 + x) * 3..(y * 32 + x) * 3 + 3].copy_from_slice(&[v, v, v]);
            }
        }
        let img = gamut_core::ImageRef::<gamut_core::Rgb8>::new(
            &rgb,
            gamut_core::Dimensions::new(32, 32).unwrap(),
        )
        .unwrap();
        let m = gamut_color::RgbToYcbcr::new(
            gamut_color::cicp::MatrixCoefficients::Bt709,
            gamut_color::cicp::ColorRange::Full,
            gamut_color::BitDepth::Eight,
        )
        .unwrap();
        Planar8::from_rgb8_matrix_subsampled(img, m, ss).unwrap()
    }

    #[test]
    fn a_vertical_split_needs_both_halves_smooth_and_a_textured_whole() {
        // `decide_rect` reaches its `vert_ok` answer only where PARTITION_HORZ is already refused,
        // so the vertical edge test is observable in exactly one situation: a block whose whole
        // extent and both horizontal halves are textured while its left and right halves are each
        // flat. It also has to be a layout where the taller-than-wide half is codable at all —
        // §6.10.4 rules PARTITION_VERT out entirely at 4:2:2, which is why every layout tested
        // here is 4:4:4 or 4:2:0.
        //
        // Both answers reconstruct bit-exactly, so the dav1d/libaom recon oracles cannot see this
        // decision: only a direct test separates the real edge test from one that measures the
        // left half twice, that accepts a single smooth half, or that has either comparison
        // inverted.
        for ss in [ChromaSubsampling::Cs444, ChromaSubsampling::Cs420] {
            for bw in [16usize, 32] {
                let hp = bw / 2;

                // A hard edge on the block's midline. Whole block and both horizontal halves span
                // it (range 180 > 32, so no PARTITION_NONE and no PARTITION_HORZ); each vertical
                // half is constant (range 0), so PARTITION_VERT is the answer.
                let edge = flat_chroma_luma(bw, ss, |x, _| if x < hp { 20 } else { 200 });
                assert_eq!(
                    FrameEncoder::new(&edge, 40).decide_rect(0, 0, bw),
                    Some(2),
                    "{ss:?} {bw}x{bw}: a midline edge with two flat halves is PARTITION_VERT"
                );

                // The same flat left half, but a right half textured by alternating rows. The
                // whole block (range 100) and both horizontal halves still exceed the threshold,
                // so PARTITION_NONE and PARTITION_HORZ stay refused — and PARTITION_VERT must be
                // refused too, because only the left half is smooth.
                let one_half = flat_chroma_luma(bw, ss, |x, y| match (x < hp, y % 2) {
                    (true, _) | (false, 0) => 20,
                    (false, _) => 120,
                });
                assert_eq!(
                    FrameEncoder::new(&one_half, 40).decide_rect(0, 0, bw),
                    None,
                    "{ss:?} {bw}x{bw}: one smooth half is not a vertical edge"
                );
            }
        }
    }

    /// An `n x n` frame in `ss` whose luma is `luma(x, y)` and whose chroma planes are flat.
    fn flat_chroma_luma(
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
        Planar8::from_planes_subsampled(n as u32, n as u32, ss, [y, chroma.clone(), chroma])
            .expect("valid planes")
    }

    /// `select_cfl`'s answer for the 8x8 block at the origin of `planes`, its luma coded `DC_PRED`
    /// over one 8x8 transform.
    fn cfl_at_origin(planes: &Planar8, speed: Speed) -> Option<(i32, i32)> {
        at_speed(planes, 40, speed).select_cfl(0, 0, 8, DC_LUMA, TxSize::Tx8x8)
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
        let (au, av) = cfl_at_origin(&up, Speed::Thorough).expect("correlated chroma wants CfL");
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
            cfl_at_origin(&down, Speed::Thorough).expect("anti-correlated chroma wants CfL");
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
        assert_eq!(cfl_at_origin(&flat, Speed::Thorough), None);
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
        assert_eq!(cfl_at_origin(&u_only, Speed::Thorough), Some((2, 0)));
        let v_only = planes_420_with(16, 16, |p, x, _| match p {
            0 => (100 + x * 8) as u8,
            1 => 128,
            _ => (122 + x * 4) as u8,
        });
        assert_eq!(cfl_at_origin(&v_only, Speed::Thorough), Some((0, 2)));
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
        let mut e = at_speed(&up, 40, Speed::Thorough);
        let before = Snapshot::of(&e);
        assert!(e.select_cfl(0, 0, 8, DC_LUMA, TxSize::Tx8x8).is_some());
        assert!(Snapshot::of(&e) == before, "select_cfl changed the encoder");
    }

    /// `planes`' encoder at `qindex`, running the search `speed` resolves to.
    fn at_speed(planes: &Planar8, qindex: u8, speed: Speed) -> FrameEncoder<'_> {
        FrameEncoder::new(planes, qindex).with_search(Search::for_speed(speed))
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
        assert_eq!(cfl_at_origin(&up, Speed::Fast), Some((2, 2)));
        assert_eq!(cfl_at_origin(&up, Speed::Fastest), None);

        // Chroma rising 6 per step wants alpha 3, which the thinned ladder does not hold: `Fast`
        // must settle on a ladder rung instead.
        let steep = planes_420_with(16, 16, |p, x, _| {
            if p == 0 {
                (100 + x * 8) as u8
            } else {
                (119 + x * 6) as u8
            }
        });
        assert_eq!(cfl_at_origin(&steep, Speed::Thorough), Some((3, 3)));
        let (au, _) =
            cfl_at_origin(&steep, Speed::Fast).expect("correlated chroma still wants CfL");
        assert!([2, 4].contains(&au), "alpha {au} is not on the Fast ladder");
    }

    /// `select_tx_type`'s choice for the luma 4x4 at the origin of a mid-grey frame at `qindex`
    /// whose residual is `res` (the prediction is the source minus `res`).
    fn pick_tx(qindex: u8, speed: Speed, res: &[i32; 16]) -> TxChoice {
        let p = grey4x4();
        let e = at_speed(&p, qindex, speed);
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
        e.select_tx_type(&site, &pred, res)
    }

    #[test]
    fn select_tx_type_tries_only_the_preset_types() {
        // A lone non-DC impulse is the residual IDTX codes best (see
        // `tx_type_selection_matches_set2_mapping`). `Fast` tries DCT_DCT and ADST_ADST only, and
        // `Fastest` DCT_DCT alone, so neither may answer IDTX (symbol 0).
        let mut impulse = [0i32; 16];
        impulse[5] = 220;
        assert_eq!(pick_tx(48, Speed::Thorough, &impulse).symbol, 0);
        assert!([1, 2].contains(&pick_tx(48, Speed::Fast, &impulse).symbol));
        assert_eq!(pick_tx(48, Speed::Fastest, &impulse).symbol, 1);
    }

    #[test]
    fn decide_palette_is_skipped_by_the_fastest_preset() {
        // Two luma colours over flat mid-grey chroma (the DC prediction of a block with no
        // neighbours) is a palette block under every other preset.
        let two = flat_chroma_luma(
            16,
            ChromaSubsampling::Cs444,
            |x, _| {
                if x < 8 { 30 } else { 210 }
            },
        );
        assert!(
            at_speed(&two, 40, Speed::Fast)
                .decide_palette(0, 0, 16)
                .is_some()
        );
        assert!(
            at_speed(&two, 40, Speed::Fastest)
                .decide_palette(0, 0, 16)
                .is_none()
        );
    }

    #[test]
    fn decide_rect_is_skipped_by_the_fastest_preset() {
        // A midline edge between two flat halves is PARTITION_VERT under the full search
        // (`a_vertical_split_needs_both_halves_smooth_and_a_textured_whole`).
        let edge = flat_chroma_luma(
            16,
            ChromaSubsampling::Cs444,
            |x, _| {
                if x < 8 { 20 } else { 200 }
            },
        );
        assert_eq!(
            at_speed(&edge, 40, Speed::Fast).decide_rect(0, 0, 16),
            Some(2)
        );
        assert_eq!(
            at_speed(&edge, 40, Speed::Fastest).decide_rect(0, 0, 16),
            None
        );
    }

    /// A 64x64 identity frame of high-contrast texture: every 8x8 is textured enough to split.
    fn busy64() -> Planar8 {
        let mut rgb = vec![0u8; 64 * 64 * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let (x, y) = (i % 64, i / 64);
            px.copy_from_slice(&[((x * 37) ^ (y * 91)) as u8, (x * 13 + y * 7) as u8, 128]);
        }
        Planar8::from_rgb8_identity(&rgb, 64, 64).unwrap()
    }

    #[test]
    fn encode_partition_stops_splitting_at_the_preset_floor() {
        // `Mi_Width_Log2` 0 is a 4-sample-wide block. Under the full search this texture splits
        // down to 4x4 leaves; the `Fastest` preset stops choosing a split below 8x8, and a 64x64
        // frame forces none.
        let p = busy64();
        let leaves = |speed| {
            let mut e = at_speed(&p, 40, speed);
            e.clear_block_decoded(0, 0);
            e.encode_partition(0, 0, 64);
            e.mi_bsl.iter().filter(|&&bsl| bsl == 0).count()
        };
        assert!(leaves(Speed::Thorough) > 0, "the full search reaches 4x4");
        assert_eq!(leaves(Speed::Fastest), 0, "no 4x4 leaf by choice");
    }

    /// Off-axis stripes: content directional prediction wins on, at a base angle plus a delta.
    fn stripes(x: f64, y: f64) -> f64 {
        128.0 + 90.0 * ((x * 0.8 + y * 0.55) / 2.3).sin() + 20.0 * (y / 5.0).cos()
    }

    /// Luma decisions of the square-block mode searches over a 64x64 frame (one superblock) whose
    /// luma is `luma(x, y)`, with the reconstruction primed to the source so every predictor sees
    /// real neighbours: each interior 8x8 (one 8x8 transform), then each interior 4x4.
    fn mode_decisions(speed: Speed, luma: fn(f64, f64) -> f64) -> (Vec<LumaMode>, Vec<LumaMode>) {
        const N: usize = 64;
        let mut rgb = vec![0u8; N * N * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let v = luma((i % N) as f64, (i / N) as f64);
            px.copy_from_slice(&[128, v as u8, 128]);
        }
        let p = Planar8::from_rgb8_identity(&rgb, N as u32, N as u32).unwrap();
        let mut e = at_speed(&p, 40, speed);
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
                nxn.push(e.select_luma_mode(r, c, 8, 8, TxSize::Tx8x8));
            }
        }
        for r in 1..N / 4 - 1 {
            for c in 1..N / 4 - 1 {
                four.push(e.select_luma_mode(r, c, 4, 4, TxSize::Tx4x4));
            }
        }
        (nxn, four)
    }

    #[test]
    fn select_luma_mode_refines_angles_only_under_the_full_search() {
        let (thorough, _) = mode_decisions(Speed::Thorough, stripes);
        assert!(
            thorough.iter().any(|m| m.angle_delta != 0),
            "off-axis stripes take an angle refinement somewhere: {thorough:?}"
        );
        let (balanced, _) = mode_decisions(Speed::Balanced, stripes);
        assert!(balanced.iter().all(|m| m.angle_delta == 0), "{balanced:?}");
    }

    /// Smooth, curved shading: content the recursive filter-intra predictor wins on.
    fn shading(x: f64, y: f64) -> f64 {
        128.0 + 60.0 * (x / 9.0).sin() * (y / 11.0).cos() + 0.4 * x
    }

    #[test]
    fn filter_intra_is_tried_only_below_the_fast_preset() {
        let (nxn, four) = mode_decisions(Speed::Balanced, shading);
        assert!(
            nxn.iter().any(|d| d.filter_intra.is_some())
                && four.iter().any(|d| d.filter_intra.is_some()),
            "some block of each size prefers filter-intra: {nxn:?} {four:?}"
        );
        let (nxn, four) = mode_decisions(Speed::Fast, shading);
        assert!(nxn.iter().all(|d| d.filter_intra.is_none()), "{nxn:?}");
        assert!(four.iter().all(|d| d.filter_intra.is_none()), "{four:?}");
    }

    #[test]
    fn directional_modes_are_tried_only_below_the_fastest_preset() {
        // ≥8×8 only: `Fastest` codes no lossy 4×4 block (see `Search::directional`), so the 4×4
        // search has no pruned configuration to pin.
        let (nxn, _) = mode_decisions(Speed::Fast, stripes);
        assert!(
            nxn.iter().any(|d| is_directional(d.mode)),
            "stripes pick a directional mode: {nxn:?}"
        );
        let (nxn, _) = mode_decisions(Speed::Fastest, stripes);
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
        let p = Planar8::from_rgb8_identity(&rgb, N as u32, N as u32).unwrap();
        let mut e = at_speed(&p, 60, Speed::Thorough);
        let cw = e.geom[0].coded_w;
        for y in 0..N {
            for x in 0..N {
                e.recon[0][y * cw + x] = e.sample(0, x, y) as u16;
            }
        }
        e.block_decoded.iter_mut().for_each(|b| *b = 1);
        for (r, c, bw, bh, tx) in [
            (2, 2, 8, 8, TxSize::Tx8x8),
            (2, 2, 8, 8, TxSize::Tx4x4),
            (3, 5, 4, 4, TxSize::Tx4x4),
            (2, 2, 16, 8, TxSize::Tx16x8),
        ] {
            let before = Snapshot::of(&e);
            let chosen = e.select_luma_mode(r, c, bw, bh, tx);
            assert!(
                Snapshot::of(&e) == before,
                "select_luma_mode changed the encoder"
            );
            let costs: Vec<(LumaMode, u64)> = e
                .luma_candidates(bw, bh)
                .into_iter()
                .map(|m| {
                    let pred = Pred {
                        mode: m.mode,
                        angle_delta: m.angle_delta,
                        filter_intra: m.filter_intra,
                        cfl_alpha: None,
                    };
                    let (sse, rate) = e.trial_plane(0, r, c, bw, bh, tx, pred);
                    (m, e.rd_cost(sse, rate + e.luma_mode_rate(r, c, bw, bh, m)))
                })
                .collect();
            let least = costs.iter().map(|c| c.1).min().unwrap();
            let first = costs.iter().find(|c| c.1 == least).unwrap().0;
            assert_eq!(chosen, first, "{r},{c} {bw}x{bh}");
        }
    }

    #[test]
    fn luma_candidates_follow_the_search_and_the_block_shape() {
        let p = grey4x4();
        let count = |speed, bw, bh| at_speed(&p, 40, speed).luma_candidates(bw, bh).len();
        // A rectangular block: the SMOOTH family and PAETH, whatever the search.
        assert_eq!(count(Speed::Thorough, 16, 8), 4);
        assert_eq!(count(Speed::Fastest, 8, 16), 4);
        // 4x4: all 13 modes, then five filter-intra modes under the full search.
        assert_eq!(count(Speed::Thorough, 4, 4), 13 + 5);
        assert_eq!(count(Speed::Fast, 4, 4), 13);
        // ≥8x8: five non-directional modes, 8 directional × 7 deltas, five filter-intra modes.
        assert_eq!(count(Speed::Thorough, 8, 8), 5 + 56 + 5);
        assert_eq!(count(Speed::Balanced, 16, 16), 5 + 8 + 5);
        assert_eq!(count(Speed::Fastest, 32, 32), 5);
        // Filter-intra up to 32x32 only (§5.11.24): a 64x64 block cannot signal it.
        assert_eq!(count(Speed::Thorough, 32, 32), 5 + 56 + 5);
        assert_eq!(count(Speed::Thorough, 64, 64), 5 + 56);
        // Cheapest signalling first: `DC_PRED` opens every square list, the rectangle's opens
        // with `SMOOTH_PRED`, and filter-intra closes them.
        let e = at_speed(&p, 40, Speed::Thorough);
        assert_eq!(e.luma_candidates(8, 8)[0], DC_LUMA);
        assert_eq!(e.luma_candidates(4, 4)[0], DC_LUMA);
        assert_eq!(e.luma_candidates(16, 8)[0].mode, SMOOTH_PRED);
        let last = *e.luma_candidates(8, 8).last().unwrap();
        assert_eq!((last.mode, last.filter_intra), (DC_PRED, Some(4)));
    }

    #[test]
    fn log2_rate_is_exact_at_powers_of_two_and_truncates_between() {
        for k in 0..16 {
            assert_eq!(log2_rate(1 << k), k * RATE_ONE);
        }
        // log2(3) = 1.58496…, log2(5) = 2.32193…, log2(24576) = 14.58496…, log2(32767) =
        // 14.99996…: eight fractional bits, truncated.
        assert_eq!(log2_rate(3), 405);
        assert_eq!(log2_rate(5), 594);
        assert_eq!(log2_rate(24576), 3733);
        assert_eq!(log2_rate(32767), 3839);
    }

    #[test]
    fn symbol_rate_is_minus_log2_of_the_symbols_probability() {
        // An even split costs one bit either way.
        assert_eq!(symbol_rate(&[16384, 32768], 0), RATE_ONE);
        assert_eq!(symbol_rate(&[16384, 32768], 1), RATE_ONE);
        // p = 1/4 is two bits; p = 3/4 is 0.415 bits (15 - log2(24576), truncated).
        assert_eq!(symbol_rate(&[8192, 32768], 0), 2 * RATE_ONE);
        assert_eq!(symbol_rate(&[8192, 32768], 1), 15 * RATE_ONE - 3733);
        // A middle symbol's probability is the step up to it, not its cumulative value.
        assert_eq!(symbol_rate(&[8192, 16384, 32768], 1), 2 * RATE_ONE);
        // A certain symbol is free, and an impossible one is priced as the least probable.
        assert_eq!(symbol_rate(&[32768, 32768], 0), 0);
        assert_eq!(symbol_rate(&[0, 32768], 0), 15 * RATE_ONE);
    }

    #[test]
    fn eob_pt_class_names_each_coefficient_count_and_refuses_others() {
        for (area, class) in [
            (16, 0),
            (32, 1),
            (64, 2),
            (128, 3),
            (256, 4),
            (512, 5),
            (1024, 6),
        ] {
            assert_eq!(eob_pt_class(area), class);
        }
        assert!(std::panic::catch_unwind(|| eob_pt_class(48)).is_err());
    }

    #[test]
    fn cfl_sign_is_the_spec_sign_code() {
        assert_eq!([cfl_sign(0), cfl_sign(-3), cfl_sign(5)], [0, 1, 2]);
    }

    #[test]
    fn a_tallying_coder_prices_instead_of_coding() {
        let mut cdf = cdf::CdfContext::new(0);
        let mut coder = Coder {
            tally: Some(0),
            ..Coder::default()
        };
        coder.encode(1, cdf.skip[0].slot());
        coder.encode_symbol(0, &[8192, 32768]);
        coder.encode_literal(5, 3);
        let skip = symbol_rate(cdf::CdfContext::new(0).skip[0].probs(), 1);
        assert_eq!(coder.tally, Some(skip + 2 * RATE_ONE + 3 * RATE_ONE));
        // Nothing reached the range coder, and the CDF did not adapt.
        assert_eq!(coder.range.finish(), Coder::default().range.finish());
        assert_eq!(
            format!("{:?}", cdf.skip[0]),
            format!("{:?}", cdf::CdfContext::new(0).skip[0])
        );
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
        // `DC_PRED` at 64x64 has a palette flag but no filter-intra flag.
        assert_eq!(
            e.luma_mode_rate(0, 0, 64, 64, DC_LUMA),
            y(DC_PRED) + no_palette(6)
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

    /// A 16x16 identity frame whose luma is `luma(x, y)` and whose chroma is flat mid-grey.
    fn luma16(luma: impl Fn(usize, usize) -> u8) -> Planar8 {
        flat_chroma_luma(16, ChromaSubsampling::Cs444, luma)
    }

    #[test]
    fn an_eight_by_eight_split_is_decided_by_trialling_both_partitions() {
        // `choose_split` at 8x8 must answer exactly "the four 4x4 leaves cost less than one 8x8
        // block", each priced whole — partition symbol, mode info, every plane — and leave the
        // encoder untouched. High-contrast texture and a gentle ramp land on either side of it.
        let ramp = luma16(|x, y| (100 + x + y) as u8);
        let busy = busy64();
        let mut seen = [false; 2];
        let at = [(0, 0), (2, 4), (6, 2), (4, 8), (10, 6)];
        for (planes, at) in std::iter::once((&ramp, (0, 0))).chain(at.map(|a| (&busy, a))) {
            let (r, c) = at;
            let mut e = at_speed(planes, 40, Speed::Thorough);
            e.clear_block_decoded(0, 0);
            // Every neighbour reconstructed as the source, as a decoder would have it at a high
            // enough quality.
            for (plane, g) in e.geom.iter().enumerate() {
                for y in 0..g.coded_h {
                    for x in 0..g.coded_w {
                        e.recon[plane][y * g.coded_w + x] = e.sample(plane, x, y) as u16;
                    }
                }
            }
            let ctx = e.partition_ctx(r, c, 1);
            let none = e.trial_cost(r, c, 8, |e| {
                e.sym.encode(0, e.cdfs.partition(1, ctx));
                e.update_partition_ctx(r, c, 1, 0);
                e.encode_block(r, c, 8, 8);
            });
            let split = e.trial_cost(r, c, 8, |e| {
                e.sym.encode(3, e.cdfs.partition(1, ctx));
                e.update_partition_ctx(r, c, 1, 3);
                for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    e.encode_partition(r + dr, c + dc, 4);
                }
            });
            let before = Snapshot::of(&e);
            let chosen = e.choose_split(r, c, 8, ctx);
            assert_eq!(chosen, split < none, "{at:?}: none {none}, split {split}");
            assert!(
                Snapshot::of(&e) == before,
                "choose_split changed the encoder"
            );
            seen[usize::from(chosen)] = true;
        }
        assert_eq!(seen, [true, true], "the cases reach both answers");
        // A search that never splits an 8x8 block does not trial one, and a larger block keeps
        // the texture test.
        let mut fastest = at_speed(&busy, 40, Speed::Fastest);
        assert!(!fastest.choose_split(0, 0, 8, 0));
        let mut e = at_speed(&busy, 40, Speed::Thorough);
        assert_eq!(e.choose_split(0, 0, 16, 0), e.should_split(0, 0, 16));
    }

    #[test]
    fn a_trial_puts_back_everything_coding_a_superblocks_first_block_changes() {
        // The first block of a superblock also signals `delta_q`/`delta_lf`, moving the quantizer
        // state; a trial of it must restore that too, along with every per-MI map, context and
        // reconstructed sample it wrote, and must neither code nor adapt anything.
        let p = busy64();
        let mut e = at_speed(&p, 40, Speed::Thorough);
        e.clear_block_decoded(0, 0);
        e.read_deltas = true;
        let before = Snapshot::of(&e);
        let cost = e.trial_cost(0, 0, 8, |e| e.encode_block(0, 0, 8, 8));
        assert!(cost > 0);
        assert!(Snapshot::of(&e) == before, "trial_cost changed the encoder");
    }

    /// Everything a trial must leave as it found it, comparable.
    #[derive(PartialEq)]
    struct Snapshot {
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
        fn of(e: &FrameEncoder<'_>) -> Self {
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
    fn planes_420_with(w: u32, h: u32, f: impl Fn(usize, usize, usize) -> u8) -> Planar8 {
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
            w,
            h,
            ChromaSubsampling::Cs420,
            [luma, chroma(1), chroma(2)],
        )
        .unwrap()
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
    fn a_two_colour_block_gets_a_palette() {
        // `decide_palette` is a pure encoder decision. Two luma colours over flat chroma is the
        // case it exists for; a version that always returned `None` would still produce a
        // conformant stream, so only a direct assertion can distinguish them.
        //
        // Three bands: the block at MI (2, 2) — luma (8, 8) — spans only the middle and right
        // ones, so it sees exactly two colours. The extra band at x < 4 exists so that reading the
        // block at a *scaled* rather than offset position picks up a third colour instead of
        // coincidentally reproducing the same set.
        // Luma: three bands, so the block at MI (2, 2) — luma (8, 8) — sees exactly two colours
        // and reading it at a scaled position would pick up the third.
        // Chroma: flat only *inside* the block's own chroma extent (4..8 on both axes), so a
        // mis-positioned chroma read sees 60 instead of 128 and the flatness check rejects it.
        let two = planes_420_with(16, 16, |p, x, y| {
            if p == 0 {
                if x < 4 {
                    50
                } else if x < 12 {
                    30
                } else {
                    210
                }
            } else if (4..8).contains(&x) && (4..8).contains(&y) {
                128
            } else {
                60
            }
        });
        let mut e = FrameEncoder::new(&two, 40);
        // A palette block's chroma must also equal its DC prediction, which reads the
        // reconstruction — empty here, so the neighbours are primed to the flat chroma value.
        for plane in 1..3 {
            e.recon[plane].fill(128);
        }
        let pal = e.decide_palette(2, 2, 8).expect("two colours ⇒ palette");
        assert_eq!(pal.colors, vec![30, 210], "sorted distinct luma colours");
        assert_eq!(pal.index_map.len(), 64);
        // Luma 8..12 is the low colour and 12..16 the high one, so the map splits mid-block.
        assert_eq!(pal.index_map[0], 0);
        assert_eq!(pal.index_map[3], 0);
        assert_eq!(pal.index_map[4], 1);
        assert_eq!(pal.index_map[7], 1);

        // Too many distinct colours to be worth a palette.
        let many = planes_420_with(
            16,
            16,
            |p, x, y| {
                if p == 0 { (x * 16 + y * 3) as u8 } else { 128 }
            },
        );
        let e = FrameEncoder::new(&many, 40);
        assert!(e.decide_palette(0, 0, 8).is_none());
    }

    #[test]
    fn palette_reconstruction_writes_each_plane_at_its_own_geometry() {
        // The luma map is written at luma resolution and chroma gets a flat DC over its *own*
        // residual block — writing the luma extent at the luma position would overrun a subsampled
        // chroma plane, and did before this was fixed.
        //
        // Driven at MI (2, 2) — luma base (8, 8), chroma base (4, 4) — because at the origin every
        // shift and division yields zero and none of them is observable.
        // Luma: three bands, so the block at MI (2, 2) — luma (8, 8) — sees exactly two colours
        // and reading it at a scaled position would pick up the third.
        // Chroma: flat only *inside* the block's own chroma extent (4..8 on both axes), so a
        // mis-positioned chroma read sees 60 instead of 128 and the flatness check rejects it.
        let two = planes_420_with(16, 16, |p, x, y| {
            if p == 0 {
                if x < 4 {
                    50
                } else if x < 12 {
                    30
                } else {
                    210
                }
            } else if (4..8).contains(&x) && (4..8).contains(&y) {
                128
            } else {
                60
            }
        });
        let mut e = FrameEncoder::new(&two, 40);
        for plane in 1..3 {
            e.recon[plane].fill(128);
        }
        let pal = e.decide_palette(2, 2, 8).expect("palette");
        // Primed non-zero so the context clear the palette path performs is observable at all — it
        // writes zeros, which is indistinguishable from not writing on a fresh encoder.
        e.above_level[0].fill(9);
        e.left_level[0].fill(9);
        e.recon_palette(2, 2, 8, &pal);
        // The block spans MI columns 2..4, so those are cleared and the rest keep their priming.
        assert_eq!(e.above_level[0][0], 9, "outside the block");
        assert_eq!(e.above_level[0][1], 9);
        assert_eq!(
            e.above_level[0][2], 0,
            "the block's own MI cells are cleared"
        );
        assert_eq!(e.above_level[0][3], 0);
        assert_eq!(e.left_level[0][2], 0);
        assert_eq!(e.left_level[0][3], 0);

        let lw = e.geom[0].coded_w;
        assert_eq!(e.recon[0][8 * lw + 8], 30, "luma base is (8, 8)");
        assert_eq!(e.recon[0][8 * lw + 15], 210);
        assert_eq!(e.recon[0][15 * lw + 15], 210);
        assert_eq!(e.recon[0][0], 0, "nothing written outside the block");

        // Chroma: a flat 4x4 DC block at the plane's own base, and nothing outside it.
        let cw = e.geom[1].coded_w;
        // The DC is the flat chroma value, written over the block's own 4x4 chroma extent.
        for i in 4..8 {
            for j in 4..8 {
                assert_eq!(e.recon[1][i * cw + j], 128, "chroma ({j},{i})");
            }
        }
        // The surrounding samples were primed to 128 too, so a mis-placed write is caught by the
        // *luma* assertions above and by the block's position rather than by a stray value here.
        // What this pins is that the write stayed within a 4x4 extent at the chroma base: a luma
        // extent (8x8) at the luma base (8, 8) would run past the 8x8 coded chroma plane entirely.
        assert_eq!(e.recon[1].len(), e.geom[1].len());
    }

    #[test]
    fn cfl_luma_is_a_shifted_sum_of_the_covered_samples() {
        // §7.11.5: `L = (sum of the (1<<subX) x (1<<subY) covered luma samples) << (3 - subX - subY)`.
        // Exact at every layout — writing it as "average, then << 3" truncates. With alpha = 0 the
        // prediction is untouched, so the values are read back through `alpha = 64`, where the
        // high-frequency term is `Round2Signed(64 * (L - lumaAvg), 6) = L - lumaAvg`.
        let p = planes_420(16, 16);
        let mut e = enc420(&p);
        // Luma coded grid is 16x16. Give the four samples under chroma (0,0) distinct values whose
        // sum is 40, and make every other covered quad sum to the same, so lumaAvg is uniform and
        // the residual isolates the first sample.
        for v in e.recon[0].iter_mut() {
            *v = 10;
        }
        e.recon[0][0] = 1;
        e.recon[0][1] = 2;
        e.recon[0][16] = 3;
        e.recon[0][17] = 4;
        // 2x2 chroma block at chroma (0,0).
        let mut pred = vec![100i32; 4];
        e.apply_cfl(&mut pred, 0, 0, 64, TxSize::Tx4x4);
        // L[0][0] = (1+2+3+4) << (3-1-1) = 10 << 1 = 20. Every other L = (10*4) << 1 = 80.
        // lumaAvg = Round2(20 + 15*80, 4) = Round2(1220, 4) = 76.
        // pred[0] = clip(100 + (20 - 76)) = 44.
        assert_eq!(pred[0], 44);
        // A sample over uniform luma sits at 100 + (80 - 76) = 104.
        assert_eq!(pred[1], 104);
    }

    /// §7.11.5 transcribed independently of the encoder, for cross-checking `apply_cfl`.
    #[allow(clippy::too_many_arguments)]
    fn reference_cfl(
        recon_luma: &[u16],
        luma_stride: usize,
        max_luma_w: usize,
        max_luma_h: usize,
        sub_x: usize,
        sub_y: usize,
        sx: usize,
        sy: usize,
        w: usize,
        h: usize,
        alpha: i32,
        dc: i32,
    ) -> Vec<i32> {
        let mut l = vec![0i32; w * h];
        let mut sum = 0i32;
        for i in 0..h {
            let luma_y = ((sy + i) << sub_y).min(max_luma_h - (1 << sub_y));
            for j in 0..w {
                let luma_x = ((sx + j) << sub_x).min(max_luma_w - (1 << sub_x));
                let mut t = 0i32;
                for dy in 0..=sub_y {
                    for dx in 0..=sub_x {
                        t += i32::from(recon_luma[(luma_y + dy) * luma_stride + (luma_x + dx)]);
                    }
                }
                let v = t << (3 - sub_x - sub_y);
                l[i * w + j] = v;
                sum += v;
            }
        }
        let shift = w.trailing_zeros() + h.trailing_zeros();
        let luma_avg = (sum + (1 << (shift - 1))) >> shift;
        l.iter()
            .map(|&lv| (dc + round2_signed(alpha * (lv - luma_avg), 6)).clamp(0, 255))
            .collect()
    }

    #[test]
    fn cfl_matches_an_independent_transcription_of_the_spec() {
        // Cross-checks `apply_cfl` against §7.11.5 written out separately, over configurations that
        // exercise the box sum, the shift, and the `MaxLuma` edge clamp — including positions where
        // the clamp actually bites, which a zero alpha would hide.
        let p = planes_420(16, 16);
        let mut e = enc420(&p);
        for (i, v) in e.recon[0].iter_mut().enumerate() {
            *v = ((i * 7) % 251) as u16;
        }
        let stride = e.geom[0].coded_w;
        let luma = e.recon[0].clone();

        for (max_w, max_h) in [(16usize, 16usize), (16, 8), (8, 16), (8, 8)] {
            e.max_luma_w = max_w;
            e.max_luma_h = max_h;
            for (sx, sy) in [(0usize, 0usize), (0, 2), (2, 0), (2, 2)] {
                for alpha in [-16i32, -1, 0, 8, 64] {
                    let dc = 100;
                    let mut got = vec![dc; 16];
                    e.apply_cfl(&mut got, sx, sy, alpha, TxSize::Tx4x4);
                    let want =
                        reference_cfl(&luma, stride, max_w, max_h, 1, 1, sx, sy, 4, 4, alpha, dc);
                    assert_eq!(
                        got, want,
                        "max=({max_w},{max_h}) at ({sx},{sy}) alpha {alpha}"
                    );
                }
            }
        }

        // The same at 4:4:4, where both shifts are zero and `L` is the luma sample times eight.
        let p444 =
            Planar8::from_planes(16, 16, [vec![0; 256], vec![0; 256], vec![0; 256]]).unwrap();
        let mut e444 = FrameEncoder::new(&p444, 40);
        for (i, v) in e444.recon[0].iter_mut().enumerate() {
            *v = ((i * 5) % 251) as u16;
        }
        let luma444 = e444.recon[0].clone();
        let stride444 = e444.geom[0].coded_w;
        e444.max_luma_w = 16;
        e444.max_luma_h = 16;
        let mut got = vec![120; 16];
        e444.apply_cfl(&mut got, 4, 4, 32, TxSize::Tx4x4);
        let want = reference_cfl(&luma444, stride444, 16, 16, 0, 0, 4, 4, 4, 4, 32, 120);
        assert_eq!(got, want);
    }

    /// A 4:2:0 encoder over a 17x13 frame: coded luma 24x16, coded chroma 12x8, MI grid 6x4.
    fn enc420(planes: &Planar8) -> FrameEncoder<'_> {
        FrameEncoder::new(planes, 40)
    }

    fn planes_420(w: u32, h: u32) -> Planar8 {
        let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
        Planar8::from_planes_subsampled(
            w,
            h,
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
        // a guard computing `x4 * k` would stop early and miss it. Four rows from y4 = 1 reach
        // exactly the 4-row edge, so the row guard must stop *before* index 4; `<=` reads one past
        // the end of the array. top = 63 and left = 5, both non-zero and both above 3 ⇒ rung 6.
        assert_eq!(e.txb_skip_ctx(0, 2, 1, 32, 32, 16, 16), 6);
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
    }

    #[test]
    fn plane_block_gives_each_plane_its_own_base_and_extent() {
        // The MI position is scaled into the plane *before* becoming samples: at MI column 3 a
        // 4:2:0 chroma block starts at sample (3 >> 1) * 4 = 4, not (3 * 4) >> 1 = 6.
        let p = planes_420(17, 13);
        let e = enc420(&p);
        assert_eq!(e.plane_block(0, 0, 8, 8, 0), Some((0, 0, 8, 8)));
        assert_eq!(e.plane_block(0, 0, 8, 8, 1), Some((0, 0, 4, 4)));
        assert_eq!(e.plane_block(2, 3, 8, 8, 0), Some((12, 8, 8, 8)));
        assert_eq!(e.plane_block(2, 3, 8, 8, 1), Some((4, 4, 4, 4)));
        // A shape with no chroma residual reports none rather than a zero-sized block.
        let p422 = Planar8::from_planes_subsampled(
            16,
            16,
            ChromaSubsampling::Cs422,
            [vec![0; 256], vec![0; 128], vec![0; 128]],
        )
        .unwrap();
        let e422 = FrameEncoder::new(&p422, 40);
        assert_eq!(e422.plane_block(0, 0, 8, 16, 1), None);
        assert_eq!(e422.plane_block(0, 0, 16, 8, 1), Some((0, 0, 8, 8)));
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
    fn prediction_references_clamp_to_the_last_sample_of_the_coded_plane() {
        // `max_x`/`max_y` are `coded_w - 1` / `coded_h - 1`, and the `- 1` is load-bearing: one
        // past the last column is not out of bounds, it is the *first sample of the next row*, so
        // an off-by-one here silently substitutes a neighbouring sample instead of panicking.
        //
        // Driving the three reference builders directly is what makes the clamp observable: a
        // whole-image encode only reaches it for blocks at the frame edge whose reference run
        // overhangs the coded grid, which no fixture currently produces.
        let planes = Planar8::from_rgb8_identity(&[0u8; 8 * 8 * 3], 8, 8).unwrap();
        let mut e = FrameEncoder::new(&planes, 40);
        assert_eq!((e.geom[0].coded_w, e.geom[0].coded_h), (8, 8));
        // Distinct value per sample, so replicating the wrong one is always visible.
        for (i, v) in e.recon[0].iter_mut().enumerate() {
            *v = i as u16;
        }
        let cw = e.geom[0].coded_w;
        let at = |x: usize, y: usize| (y * cw + x) as i32;

        // (6, 6) on an 8x8 coded grid: the 4-sample reference run reaches column/row 9, so both
        // clamps bite. Under `coded_w + 1` or `coded_w / 1` the clamped reads would wrap to the
        // next row instead of repeating column 7.
        let (above, left, corner) = e.reference_4x4(0, 6, 6);
        assert_eq!(above[0], at(6, 5));
        assert_eq!(above[1], at(7, 5));
        assert_eq!(
            above[2],
            at(7, 5),
            "past the right edge repeats the last column"
        );
        assert_eq!(above[7], at(7, 5));
        assert_eq!(left[0], at(5, 6));
        assert_eq!(left[1], at(5, 7));
        assert_eq!(
            left[2],
            at(5, 7),
            "past the bottom edge repeats the last row"
        );
        assert_eq!(corner, at(5, 5));

        let (above, left, _) = e.reference_directional(0, 6, 6, 4);
        assert_eq!(above[1], at(7, 5));
        assert_eq!(above[2], at(7, 5));
        assert_eq!(left[1], at(5, 7));
        assert_eq!(left[2], at(5, 7));

        // `reference_basic` clamps per sample with no separate limit, so a block whose width and
        // height overhang the grid exercises both bounds directly.
        let (above, left, _) = e.reference_basic(0, 4, 4, 8, 8);
        assert_eq!(above[3], at(7, 3));
        assert_eq!(
            above[4],
            at(7, 3),
            "past the right edge repeats the last column"
        );
        assert_eq!(above[7], at(7, 3));
        assert_eq!(left[3], at(3, 7));
        assert_eq!(
            left[4],
            at(3, 7),
            "past the bottom edge repeats the last row"
        );
        assert_eq!(left[7], at(3, 7));
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
            17,
            13,
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
    fn planes_with(
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
        Planar8::from_planes(w, h, [y, c.clone(), c]).expect("valid 4:4:4 planes")
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
    fn palette_is_chosen_only_for_two_to_eight_luma_colours() {
        // Like skippability, palette selection is a pure quality decision: declining it always
        // reconstructs correctly, so nothing in the oracle suite distinguishes "no palette here"
        // from "never any palette". §5.11.46's 2..=8 bound is the contract worth pinning.
        //
        // The block is MI (2, 2) — samples (8, 8) to (15, 15) — so both neighbours exist and the
        // zero chroma matches its DC prediction.
        // The content is position-keyed: only the samples of MI (2, 2) — x and y both in 8..16 —
        // carry the pair {50, 150}, in a checkerboard, and everything outside carries values
        // distinct from both and from each other. So the palette and its index map are unique to
        // *this* block, and any misaddressing — `c * 4` read as `c + 4`, `sx + j` as `sx * j`, a
        // transposed row and column — samples something else and produces a different answer.
        let keyed = |x: u32, y: u32| -> u8 {
            if (8..16).contains(&x) && (8..16).contains(&y) {
                if (x + y).is_multiple_of(2) { 50 } else { 150 }
            } else {
                (200 + (x % 5) * 10 + (y % 3)) as u8
            }
        };
        let e_planes = planes_with(32, 32, keyed, |_, _| 0);
        let e = FrameEncoder::new(&e_planes, 40);
        let pal = e
            .decide_palette(2, 2, 8)
            .expect("a two-colour block is a palette");
        assert_eq!(pal.colors, vec![50, 150], "sorted distinct colours");
        assert_eq!(pal.index_map.len(), 64);
        // The map is the checkerboard itself: index 0 is 50 (the lower colour), 1 is 150. Row and
        // column enter it differently, so a transposed read shows up as a shifted parity.
        let want: Vec<u8> = (0..8u32)
            .flat_map(|i| (0..8u32).map(move |j| u8::from(!(i + j).is_multiple_of(2))))
            .collect();
        assert_eq!(pal.index_map, want);

        let two = |x: u32, _: u32| if x < 12 { 0 } else { 200 };

        // One colour is below the bound, nine is above it.
        let e_planes = planes_with(32, 32, |_, _| 0, |_, _| 0);
        let e = FrameEncoder::new(&e_planes, 40);
        assert!(
            e.decide_palette(2, 2, 8).is_none(),
            "one colour is not a palette"
        );
        let e_planes = planes_with(32, 32, |x, y| ((x + y * 8) % 9) as u8, |_, _| 0);
        let e = FrameEncoder::new(&e_planes, 40);
        assert!(
            e.decide_palette(2, 2, 8).is_none(),
            "nine colours exceed the bound"
        );

        // Non-flat chroma disqualifies a block whose luma qualifies — the precondition that is
        // load-bearing at 4:4:4 and vacuous for a monochrome frame.
        let e_planes = planes_with(32, 32, two, |x, y| u8::from(x == 10 && y == 10));
        let e = FrameEncoder::new(&e_planes, 40);
        assert!(
            e.decide_palette(2, 2, 8).is_none(),
            "a non-flat chroma plane disqualifies the block"
        );
    }

    #[test]
    fn palette_delta_bits_narrows_to_the_remaining_range() {
        // §5.11.46: after coding a colour, `paletteBits` narrows to `CeilLog2((1 << BitDepth) -
        // colour - 1)`. Too *wide* a width still round-trips for any palette whose later deltas
        // happen to fit, so only the colours at a `CeilLog2` boundary tell the arithmetic apart.
        //
        // At 8 bits colour 0 leaves 255, needing all 8; colour 127 leaves 128, needing 7. That
        // second one is the discriminating case: a `- 1` written as `+ 1` leaves 130, and a
        // `(1 << BitDepth) - colour` written as `+` leaves 382 — both `CeilLog2` 8, not 7.
        assert_eq!(palette_delta_bits(8, 0, 8), 8);
        assert_eq!(palette_delta_bits(8, 127, 8), 7);
        assert_eq!(palette_delta_bits(8, 128, 8), 7);
        // The top of the range: 254 leaves 1 and 255 leaves 0, both `CeilLog2` 0 — the `max(1)`
        // that keeps `CeilLog2(0)` from being asked for.
        assert_eq!(palette_delta_bits(8, 254, 8), 0);
        assert_eq!(palette_delta_bits(8, 255, 8), 0);
        // It only ever narrows, never widens back.
        assert_eq!(palette_delta_bits(8, 0, 5), 5);
        // And the range is the *coded depth's*, so the same colour narrows differently at 12 bits.
        assert_eq!(palette_delta_bits(12, 128, 12), 12);
        assert_eq!(palette_delta_bits(12, 2048, 12), 11);
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

    #[test]
    fn ceil_log2_matches_spec() {
        // CeilLog2(x) = smallest k with 2^k >= x (0 for x <= 1). Boundaries at each power of two pin
        // the `< 2` guard, the `x - 1`, and the `+ 1`.
        for (x, want) in [
            (0usize, 0u32),
            (1, 0),
            (2, 1),
            (3, 2),
            (4, 2),
            (5, 3),
            (7, 3),
            (8, 3),
            (9, 4),
            (16, 4),
            (17, 5),
        ] {
            assert_eq!(ceil_log2(x), want, "ceil_log2({x})");
        }
    }

    #[test]
    fn recenter_codes_the_expected_value() {
        // recenter maps (ref, v) to the value encode_subexp actually codes; the dav1d oracle only sees
        // v == ref (a zero Wiener delta), so the `v > 2r` and `v < r` branches are otherwise untested.
        // Pin the exact coded value per branch. (A round-trip is too weak here: `inverse_recenter`
        // recovers v even from a wrongly-coded value, so it cannot see an output change.) The `>` at
        // v == 2*ref is a continuity point — both `>` and `>=` yield 2*ref — and is excluded as equivalent.
        for (r, v, want) in [
            (5, 12, 12u32), // v > 2r        -> v
            (5, 11, 11),    // v > 2r
            (5, 2, 5),      // v < r         -> 2(r-v)-1
            (5, 0, 9),      // v < r
            (5, 5, 0),      // v == r        -> 2(v-r)
            (5, 7, 4),      // r < v <= 2r   -> 2(v-r)
            (5, 10, 10),    // v == 2r       (continuity)
            (0, 3, 3),      // ref 0
            (3, 0, 5),      // v < r
        ] {
            assert_eq!(recenter(r, v), want, "recenter({r}, {v})");
        }
    }

    #[test]
    fn subexp_coding_is_stable() {
        // encode_subexp_with_ref drives recenter -> encode_subexp -> encode_ns (§4.10.7-9). The encoder
        // only codes a zero Wiener delta (v == ref), so every non-trivial path of this sub-exponential
        // coder is unexercised by the dav1d oracle. Pin a checksum of the coded bits for a spread of
        // (v, mx, k, ref) covering both reference halves, the more-bits loop, the NS escape and the
        // final literal. Any change to the coding moves the checksum; the v == ref path's correctness
        // is proven by the loop-restoration oracle, so the pair is deterministic AND correct.
        fn fnv1a(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let mut sym = SymbolEncoder::new();
        // encode_ns directly: cover v < m, v == m and v > m (m = (1<<FloorLog2(n)+1) - n).
        for &(v, n) in &[
            (0u32, 5u32),
            (2, 5),
            (3, 5),
            (4, 5), // n=5: m=3
            (1, 3),
            (2, 3), // n=3: m=1
            (1, 7),
            (5, 7), // n=7: m=1
            (2, 6),
            (5, 6), // n=6: m=2
            (0, 1),
            (3, 8),
            (7, 8),
        ] {
            encode_ns(&mut sym, v, n);
        }
        // encode_subexp directly: cover the NS escape, the more-bits loop, and the final literal with a
        // non-zero `mk` (v=9,k=2 iterates twice then settles in the else with mk=8).
        for &(v, ns, k) in &[
            (0u32, 16u32, 1u32),
            (9, 64, 2),
            (3, 16, 0),
            (20, 64, 1),
            (1, 4, 0),
            (50, 64, 3),
            (7, 32, 2),
        ] {
            encode_subexp(&mut sym, v, ns, k);
        }
        // encode_subexp_with_ref: both reference halves ((r<<1) <= mx and not).
        for &(v, mx, k, r) in &[
            (0i32, 16i32, 1u32, 0i32),
            (5, 16, 1, 3),
            (20, 32, 1, 25),
            (62, 64, 2, 60),
            (33, 64, 0, 1),
            (2, 5, 1, 4),
        ] {
            encode_subexp_with_ref(&mut sym, v, mx, k, r);
        }
        assert_eq!(fnv1a(&sym.finish()), 0xd0fc_c97e_4152_8bd6);
    }

    /// A 4×4 mid-grey image is enough to construct an encoder; selection depends only on the
    /// residual and the quantizers, not the plane contents.
    fn grey4x4() -> Planar8 {
        Planar8::from_rgb8_identity(&[128u8; 4 * 4 * 3], 4, 4).unwrap()
    }

    #[test]
    fn block_exceeds_frame_flags_only_overhang() {
        // Fully inside, with room to spare.
        assert!(!block_exceeds_frame(0, 0, 8, 16, 16));
        // Exactly reaching each edge fits (pins `>` vs `>=`, and `+` vs `*` at non-zero positions).
        assert!(!block_exceeds_frame(0, 0, 8, 8, 8)); // r+8 == 8 and c+8 == 8
        assert!(!block_exceeds_frame(2, 0, 6, 8, 8)); // r+6 == 8 exactly
        assert!(!block_exceeds_frame(0, 2, 6, 8, 8)); // c+6 == 8 exactly
        // Overhanging only the bottom edge, or only the right edge, must still flag (pins `||` vs
        // `&&`, the row/col `>` mutants, and the `c*num4x4`/`r*num4x4` mutants at position 0).
        assert!(block_exceeds_frame(0, 0, 8, 6, 99)); // rows overhang only
        assert!(block_exceeds_frame(0, 0, 8, 99, 6)); // cols overhang only
    }

    #[test]
    fn tx_type_selection_matches_set2_mapping() {
        // A flat residual concentrates into the DCT DC bin (one coefficient), so DCT_DCT wins and
        // is signaled as Tx_Type_Intra_Inv_Set2 symbol 1.
        assert_eq!(pick_tx(48, Speed::Thorough, &[40; 16]).symbol, 1);

        // A single non-DC impulse stays a single coefficient under the identity transform but
        // spreads across many DCT bins, so IDTX wins and is signaled as symbol 0.
        let mut impulse = [0i32; 16];
        impulse[5] = 220;
        let idtx = pick_tx(48, Speed::Thorough, &impulse);
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
            let choice = e.select_tx_type(&site, &pred, &res);
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

    #[test]
    fn predictors_match_spec_formulas() {
        // 12×12 mid-grey image → the coded grid has a block at (4,4) with available neighbours.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        // Above row at y=3, x=4..8; left col at x=3, y=4..8; top-left at (3,3).
        for j in 0..4 {
            e.recon[0][3 * cw + (4 + j)] = 100;
        }
        for i in 0..4 {
            e.recon[0][(4 + i) * cw + 3] = 50;
        }
        e.recon[0][3 * cw + 3] = 80;

        // PAETH: base = 100 + 50 - 80 = 70; |70-80| (top-left) is smallest, so every sample = 80.
        let paeth = e.predict_4x4(0, 4, 4, PAETH_PRED);
        assert_eq!(paeth, [80; 16]);

        // SMOOTH_V row 0 uses weight 255: (255*100 + 1*50 + 128) >> 8 = 100; row 3 weight 64:
        // (64*100 + 192*50 + 128) >> 8 = 63.
        let sv = e.predict_4x4(0, 4, 4, SMOOTH_V_PRED);
        assert_eq!(sv[0], 100);
        assert_eq!(sv[12], 63);

        // SMOOTH_H column 0 uses weight 255: (255*50 + 1*100 + 128) >> 8 = 50.
        let sh = e.predict_4x4(0, 4, 4, SMOOTH_H_PRED);
        assert_eq!(sh[0], 50);

        // SMOOTH (i=0,j=0): (255*100 + 1*50 + 255*50 + 1*100 + 256) >> 9 = 75.
        let sm = e.predict_4x4(0, 4, 4, SMOOTH_PRED);
        assert_eq!(sm[0], 75);
    }

    #[test]
    fn general_directional_matches_4x4_path() {
        // The size-generic directional predictor (used for 8×8) must reproduce the validated 4×4
        // predictor exactly when n = 4 and angle_delta = 0, for every directional mode — including
        // the zone-1/3 modes that read the above-right / below-left extension. All BlockDecoded
        // cells are marked available so both code paths read the same extended references.
        let p = Planar8::from_rgb8_identity(&[128u8; 16 * 16 * 3], 16, 16).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for y in 0..e.geom[0].coded_h {
            for x in 0..cw {
                e.recon[0][y * cw + x] = ((x * 7 + y * 13 + 5) & 0xff) as u16;
            }
        }
        e.block_decoded.iter_mut().for_each(|b| *b = 1);
        for &mode in &DIRECTIONAL_MODES {
            let general = e.predict_directional(0, 4, 4, 4, mode, 0);
            let baseline = e.predict_4x4(0, 4, 4, mode).to_vec();
            assert_eq!(general, baseline, "directional mode {mode} (n=4) mismatch");
        }
    }

    #[test]
    fn directional_8x8_angle_delta_in_range() {
        // 8×8 directional prediction with every angle_delta must stay in [0, 255] (no OOB / overflow
        // across all four zones), exercising the extended 16-sample reference arrays.
        let p = Planar8::from_rgb8_identity(&[128u8; 32 * 32 * 3], 32, 32).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for y in 0..e.geom[0].coded_h {
            for x in 0..cw {
                e.recon[0][y * cw + x] = (((x * 11) ^ (y * 5 + 9)) & 0xff) as u16;
            }
        }
        e.block_decoded.iter_mut().for_each(|b| *b = 1);
        for &mode in &DIRECTIONAL_MODES {
            for ad in -3..=3i8 {
                let pred = e.predict_directional(0, 8, 8, 8, mode, ad);
                assert_eq!(pred.len(), 64);
                assert!(
                    pred.iter().all(|&x| (0..=255).contains(&x)),
                    "8×8 mode {mode} angle_delta {ad} out of range"
                );
            }
        }
    }

    #[test]
    fn reference_sample_fallbacks_at_frame_corner() {
        // Block at (0,0): no above, no left. Defaults are AboveRow=127, LeftCol=129, top-left=128.
        let p = grey4x4();
        let e = FrameEncoder::new(&p, 32);
        // PAETH base = 127 + 129 - 128 = 128; distances are |128-129|=1 (left), |128-127|=1 (top),
        // |128-128|=0 (top-left), so the top-left default (128) wins and every sample is 128.
        assert_eq!(e.predict_4x4(0, 0, 0, PAETH_PRED), [128; 16]);
    }

    #[test]
    fn directional_predictors_match_spec() {
        // Block at (4,4) with a ramped above row [60,90,120,150], left column [40,80,120,160],
        // top-left 100.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for (j, &v) in [60u16, 90, 120, 150].iter().enumerate() {
            e.recon[0][3 * cw + (4 + j)] = v;
        }
        for (i, &v) in [40u16, 80, 120, 160].iter().enumerate() {
            e.recon[0][(4 + i) * cw + 3] = v;
        }
        e.recon[0][3 * cw + 3] = 100;

        // V (pAngle 90): every row is a copy of the above row.
        let v = e.predict_4x4(0, 4, 4, V_PRED);
        assert_eq!(&v[0..4], &[60, 90, 120, 150]);
        assert_eq!(&v[12..16], &[60, 90, 120, 150]);
        // H (pAngle 180): every column is a copy of the left column.
        let h = e.predict_4x4(0, 4, 4, H_PRED);
        assert_eq!([h[0], h[4], h[8], h[12]], [40, 80, 120, 160]);
        // D135: the main diagonal carries the top-left corner; row 0 = [TL, above0, above1, above2].
        let d135 = e.predict_4x4(0, 4, 4, D135_PRED);
        assert_eq!(&d135[0..4], &[100, 60, 90, 120]);
        assert_eq!([d135[0], d135[5], d135[10], d135[15]], [100, 100, 100, 100]);
        // All directional outputs stay in range.
        for &m in &[D113_PRED, D157_PRED] {
            assert!(
                e.predict_4x4(0, 4, 4, m)
                    .iter()
                    .all(|&x| (0..=255).contains(&x))
            );
        }
    }

    #[test]
    fn filter_intra_predictor_matches_spec() {
        // Block at (4,4) with a single non-zero reference: AboveRow[0] = 16, every other reference
        // (AboveRow[1..3], LeftCol[0..3], top-left) = 0. FILTER_DC_PRED taps then give a hand-traced
        // recursive result (the lower 4×2 filters the upper 4×2's predicted row 1, §7.11.2.3).
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        e.recon[0][3 * cw + 4] = 16; // AboveRow[0]
        let pred = e.predict_filter_intra(0, 4, 4, 4, 0);
        assert_eq!(
            pred,
            [10, 2, 1, 1, 6, 2, 2, 1, 4, 2, 2, 1, 2, 2, 2, 1],
            "FILTER_DC recursive prediction"
        );
    }

    #[test]
    fn filter_intra_preserves_flat_reference() {
        // Every Intra_Filter_Taps row sums to 16 = 1 << INTRA_FILTER_SCALE_BITS, so a constant
        // reference must reproduce that constant for all five filter-intra modes.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for k in 0..8 {
            e.recon[0][3 * cw + (4 + k)] = 77; // above row + above-right
            e.recon[0][(4 + k) * cw + 3] = 77; // left col + below-left
        }
        e.recon[0][3 * cw + 3] = 77; // top-left
        for fi in 0..5u8 {
            assert_eq!(
                e.predict_filter_intra(0, 4, 4, 4, fi),
                [77; 16],
                "filter-intra mode {fi} should preserve a flat reference"
            );
        }
    }

    #[test]
    fn cfl_prediction_matches_spec() {
        // Reconstructed luma with a pure column gradient 80,88,96,104 (constant down each column).
        // For 4:4:4, L = luma*8, lumaAvg = Round2(ΣL, 4); with alpha = 2 the per-column high-freq
        // term Round2Signed(2*(L-avg), 6) is -3,-1,+1,+3, added to a flat DC chroma prediction.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for i in 0..4 {
            for j in 0..4 {
                e.recon[0][(4 + i) * cw + (4 + j)] = (80 + 8 * j) as u16;
            }
        }
        let mut pred = [128i32; 16];
        e.apply_cfl(&mut pred, 4, 4, 2, TxSize::Tx4x4);
        assert_eq!(
            pred,
            [
                125, 127, 129, 131, 125, 127, 129, 131, 125, 127, 129, 131, 125, 127, 129, 131
            ]
        );
        // alpha = 0 is a no-op (plain DC).
        let mut flat = [100i32; 16];
        e.apply_cfl(&mut flat, 4, 4, 0, TxSize::Tx4x4);
        assert_eq!(flat, [100; 16]);
    }

    #[test]
    fn d45_reads_above_right_when_available() {
        // Block at (4,4) ⇒ superblock-relative (by, bx) = (1, 1). Mark the above-right 4×4 decoded
        // so D45 (pAngle 45) reads the real extended above-row samples 4..8.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], 12, 12).unwrap();
        let mut e = FrameEncoder::new(&p, 32);
        let cw = e.geom[0].coded_w;
        for (k, &v) in [10u16, 20, 30, 40, 50, 60, 70, 80].iter().enumerate() {
            e.recon[0][3 * cw + (4 + k)] = v; // above row incl. above-right (x=4..12)
        }
        e.clear_block_decoded(0, 0);
        e.block_decoded[BD_STRIDE + 3] = 1; // (by-1, bx+1) = (0, 2) decoded ⇒ haveAboveRight
        // dx = 64 ⇒ shift = 0, base = (i+1)+j; row 0 = above[1..5] = [20,30,40,50] (50 is an
        // above-right sample) and the bottom-right clamps to above[w+h-1] = above[7] = 80.
        let d45 = e.predict_4x4(0, 4, 4, D45_PRED);
        assert_eq!(&d45[0..4], &[20, 30, 40, 50]);
        assert_eq!(d45[15], 80);
    }
}
