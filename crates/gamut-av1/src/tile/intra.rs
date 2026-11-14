//! Intra prediction (§7.11.2): the reference-sample builders and the DC, smooth, Paeth,
//! directional and filter-intra predictors, chroma-from-luma (§7.11.5), and the luma palette
//! (§5.11.46–§5.11.50, §7.11.4) — deciding, signalling and reconstructing it.

use gamut_dsp::math::{clip_pixel, round2, round2_signed};

use super::{
    D45_PRED, D67_PRED, D113_PRED, D135_PRED, D157_PRED, D203_PRED, DC_PRED, FrameEncoder, H_PRED,
    PAETH_PRED, SMOOTH_PRED, SMOOTH_V_PRED, V_PRED,
};
use crate::cdf;
use crate::geom::plane_residual_size;
use crate::transform::TxSize;

/// `Filter_Intra_Mode_To_Intra_Dir[INTRA_FILTER_MODES]` (§9.3): maps a filter-intra mode to the
/// `intraDir` used to select the 16×16 transform-type CDF (§9.4). Order:
/// `{FILTER_DC, FILTER_V, FILTER_H, FILTER_D157, FILTER_PAETH}`.
pub(super) const FILTER_INTRA_MODE_TO_INTRA_DIR: [u8; 5] =
    [DC_PRED, V_PRED, H_PRED, D157_PRED, DC_PRED];

/// True for the eight directional intra modes (`V_PRED..=D67_PRED`, mode indices `1..=8`), which is
/// `is_directional_mode` (§7.11.2). These are the modes for which `angle_delta_y` is signaled (≥8×8)
/// and whose prediction follows the directional process (§7.11.2.4).
pub(super) const fn is_directional(mode: u8) -> bool {
    V_PRED <= mode && mode <= D67_PRED
}

/// A selected luma palette for a block (§5.11.46/.49): the sorted distinct colors and the per-pixel
/// `ColorMapY` (index into `colors`, row-major over the block).
pub(super) struct PaletteBlock {
    /// Palette entries, sorted ascending, at the frame's coded bit depth.
    pub(super) colors: Vec<u16>,
    pub(super) index_map: Vec<u8>,
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
        // The spec's shift loop (`for k = maxIdx; k > i; k--`) moves `maxIdx` to `i` and every
        // entry between them one place right: a right rotation of `i..=max_idx`, which is a no-op
        // when `max_idx == i`. Spelled as the rotation, not as a cursor stepped by its own
        // arithmetic, since `k -= 1` → `k /= 1` never moves the cursor and spins (#920); the
        // rewrite also took the loop's own operator mutants and the `max_idx != i` guard with it.
        scores[i..=max_idx].rotate_right(1);
        order[i..=max_idx].rotate_right(1);
    }
    let hash = (scores[0] + scores[1] * 2 + scores[2] * 2) as usize;
    (order, cdf::PALETTE_COLOR_CONTEXT[hash] as usize)
}

/// How a single transform block's samples are predicted. `mode` is the intra mode (`DC_PRED` for
/// chroma); `angle_delta` is the directional fine angle (`AngleDeltaY`, signaled only for ≥8×8 luma
/// directional blocks); `filter_intra` overrides luma prediction with a recursive filter-intra mode
/// (§7.11.2.3); `cfl_alpha` adds the chroma-from-luma high-frequency term (§7.11.5). At most one of
/// the latter two is set, and only on its respective plane.
#[derive(Clone, Copy)]
pub(super) struct Pred {
    pub(super) mode: u8,
    pub(super) angle_delta: i8,
    pub(super) filter_intra: Option<u8>,
    pub(super) cfl_alpha: Option<i32>,
}

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

impl FrameEncoder<'_> {
    /// Decides whether to code a lossy block at MI `(r, c)` with luma palette mode (§5.11.46): the
    /// luma block must have 2..=8 distinct colors, and (so the block can be coded `skip = 1` with the
    /// reconstruction being exactly the palette + DC chroma) every **coded** chroma plane must be
    /// flat and DC-predictable — a monochrome frame has none, so that condition is vacuous there
    /// and palette is reachable on content a 4:4:4 frame would reject. Returns the sorted palette
    /// and the per-pixel index map. This is a quality decision; any choice reconstructs
    /// bit-exactly, so only the signaling has to be correct.
    pub(super) fn decide_palette(&self, r: usize, c: usize, bw: usize) -> Option<PaletteBlock> {
        // A pruned search does not look for a palette at all.
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
    ///
    /// The spec spells this as a two-cursor merge that skips a colour equal to the last one
    /// cached. Every palette this encoder stores ascends strictly ([`Self::decide_palette`] builds
    /// it by sorted insertion of distinct samples), and for two ascending lists that merge is
    /// exactly the sorted union, so it is computed as one. The cursor form left mutants no input
    /// could tell apart — `lc < ac` against `<=`, and the spec's `leftIdx` step on equal colours
    /// against `*=`, both absorbed by the repeat check — and a `+= 1` → `*= 1` on any of its four
    /// cursors spun the walk until the test timeout.
    pub(super) fn palette_cache(&self, r: usize, c: usize) -> Vec<u16> {
        let palette = |i: usize| &self.mi_pcolors[i][..usize::from(self.mi_psize[i])];
        let above = if !r.is_multiple_of(16) {
            palette((r - 1) * self.mi_cols + c)
        } else {
            &[]
        };
        let left = if c > self.tile_c0 {
            palette(r * self.mi_cols + (c - 1))
        } else {
            &[]
        };
        let mut cache: Vec<u16> = above.iter().chain(left).copied().collect();
        cache.sort_unstable();
        cache.dedup();
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
    pub(super) fn signal_palette_colors(&mut self, colors: &[u16], cache_n: usize) {
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
    pub(super) fn signal_palette_tokens(&mut self, index_map: &[u8], bw: usize, psize: usize) {
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
    pub(super) fn recon_palette(&mut self, r: usize, c: usize, bw: usize, pal: &PaletteBlock) {
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

    /// DC intra prediction value for a 4×4 at coded position `(sx, sy)` (§7.11.2.5). In the lossless
    /// path neighbours are the (padded) source (which equals the reconstruction); in the lossy path
    /// they are the reconstruction buffer, matching the decoder exactly.
    pub(super) fn dc_avg(&self, plane: usize, sx: usize, sy: usize) -> i32 {
        self.dc_pred(plane, sx, sy, 4, 4)
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
    pub(super) fn plane_block(
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
    pub(super) fn dc_pred(&self, plane: usize, sx: usize, sy: usize, w: usize, h: usize) -> i32 {
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
    pub(super) fn predict_filter_intra(
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
    pub(super) fn predict_intra(
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
                // The branches run in angle order, each cardinal angle between the two zones it
                // separates, so no comparison overlaps an earlier branch: a `<` → `<=` on either
                // zone bound sends its cardinal angle into the zone, which indexes
                // `Dr_Intra_Derivative[90]`, one past the table.
                let v = if p_angle < 90 {
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
                } else if p_angle == 90 {
                    above[j as usize]
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
                } else if p_angle == 180 {
                    left[i as usize]
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

    /// Adds the chroma-from-luma high-frequency term to a chroma DC prediction in place (§7.11.5).
    ///
    /// `L[i][j]` is the **sum** of the `(1 << subX) x (1 << subY)` reconstructed luma samples the
    /// chroma sample covers, shifted left by `3 - subX - subY`. That is exact at every subsampling —
    /// all three layouts land on the same 3-fractional-bit scale with no rounding — so it must not
    /// be written as "average, then scale", which truncates. At 4:4:4 it reduces to `luma << 3`.
    ///
    /// `lumaAvg = Round2(ΣL, Tx_Width_Log2 + Tx_Height_Log2)`, and each chroma sample becomes
    /// `Clip1(dc + Round2Signed(alpha * (L - lumaAvg), 6))`. `alpha == 0` is a no-op (plain DC).
    pub(super) fn apply_cfl(
        &self,
        pred: &mut [i32],
        sx: usize,
        sy: usize,
        alpha: i32,
        tx_size: TxSize,
    ) {
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
}

#[cfg(test)]
mod tests {
    use gamut_color::{ChromaSubsampling, Planar8};

    use super::*;
    use crate::encoder::Effort;
    use crate::tile::mode::DIRECTIONAL_MODES;
    use crate::tile::tests::{
        at_effort, dims, enc420, flat_chroma_luma, grey4x4, planes_420, planes_420_with,
        planes_with,
    };
    use crate::tile::{BD_STRIDE, SMOOTH_H_PRED};
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
            at_effort(&two, 40, Effort::Fast)
                .decide_palette(0, 0, 16)
                .is_some()
        );
        assert!(
            at_effort(&two, 40, Effort::Fastest)
                .decide_palette(0, 0, 16)
                .is_none()
        );
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
        for plane in &mut e.recon[1..3] {
            plane.fill(128);
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
        for plane in &mut e.recon[1..3] {
            plane.fill(128);
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
            Planar8::from_planes(dims(16, 16), [vec![0; 256], vec![0; 256], vec![0; 256]]).unwrap();
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
            dims(16, 16),
            ChromaSubsampling::Cs422,
            [vec![0; 256], vec![0; 128], vec![0; 128]],
        )
        .unwrap();
        let e422 = FrameEncoder::new(&p422, 40);
        assert_eq!(e422.plane_block(0, 0, 8, 16, 1), None);
        assert_eq!(e422.plane_block(0, 0, 16, 8, 1), Some((0, 0, 8, 8)));
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
        let planes = Planar8::from_rgb8_identity(&[0u8; 8 * 8 * 3], dims(8, 8)).unwrap();
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
    fn reference_4x4_replicates_its_own_edge_without_above_right_or_below_left() {
        // §7.11.2: with the above-right (below-left) 4×4 not yet decoded, the above row (left
        // column) holds the block's own 4 samples and then repeats the 4th; the `- 1` in each
        // limit is what stops at that 4th. Away from the frame edge, so the plane clamp the test
        // above pins cannot stand in for it, and with a distinct value per sample.
        let planes = Planar8::from_rgb8_identity(&[0u8; 16 * 16 * 3], dims(16, 16)).unwrap();
        let mut e = FrameEncoder::new(&planes, 40);
        for (i, v) in e.recon[0].iter_mut().enumerate() {
            *v = i as u16;
        }
        let cw = e.geom[0].coded_w;
        let at = |x: usize, y: usize| (y * cw + x) as i32;
        assert!(!e.block_decoded_at(0, 2) && !e.block_decoded_at(2, 0));

        let (above, left, _) = e.reference_4x4(0, 4, 4);
        assert_eq!(above[3], at(7, 3));
        assert_eq!(above[4], at(7, 3), "no above-right: the 4th sample repeats");
        assert_eq!(left[3], at(3, 7));
        assert_eq!(left[4], at(3, 7), "no below-left: the 4th sample repeats");
    }

    #[test]
    fn reference_4x4_reads_below_left_only_when_that_4x4_is_decoded() {
        // §5.11.34 `haveBelowLft` asks about the 4×4 one row down and one column left of the
        // block, and no other: the left and above-left 4×4s (always decoded before it) must not
        // stand in for it, nor the one below-right (never decoded before it).
        let planes = Planar8::from_rgb8_identity(&[0u8; 16 * 16 * 3], dims(16, 16)).unwrap();
        let mut e = FrameEncoder::new(&planes, 40);
        for (i, v) in e.recon[0].iter_mut().enumerate() {
            *v = i as u16;
        }
        let cw = e.geom[0].coded_w;
        let at = |x: usize, y: usize| (y * cw + x) as i32;
        // `BlockDecoded` at superblock-relative 4×4 `(by, bx)`; the block is `(1, 1)`.
        let mark = |e: &mut FrameEncoder, by: usize, bx: usize, v: u8| {
            e.block_decoded[(by + 1) * BD_STRIDE + bx + 1] = v;
        };

        // Left and above-left decoded, below-left not: the 4th sample repeats.
        mark(&mut e, 1, 0, 1);
        mark(&mut e, 0, 0, 1);
        let (_, left, _) = e.reference_4x4(0, 4, 4);
        assert_eq!(left[4], at(3, 7));

        // Only below-left decoded: its samples are real.
        mark(&mut e, 1, 0, 0);
        mark(&mut e, 0, 0, 0);
        mark(&mut e, 2, 0, 1);
        let (_, left, _) = e.reference_4x4(0, 4, 4);
        assert_eq!(left[4], at(3, 8));
        assert_eq!(left[7], at(3, 11));
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
    fn predictors_match_spec_formulas() {
        // 12×12 mid-grey image → the coded grid has a block at (4,4) with available neighbours.
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 16 * 16 * 3], dims(16, 16)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 32 * 32 * 3], dims(32, 32)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
        let p = Planar8::from_rgb8_identity(&[128u8; 12 * 12 * 3], dims(12, 12)).unwrap();
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
