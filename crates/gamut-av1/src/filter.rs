//! The in-loop filters and post-processing applied to the reconstructed frame after all blocks are
//! decoded, in the decoder's order (§7.4): the **deblocking loop filter** (§7.14) then **CDEF**
//! (§7.15, deringing), then optional **superres** horizontal upscaling (§7.16) and **loop
//! restoration** (§7.17, luma Wiener). They are pure post-processes — intra prediction during
//! encoding reads the *pre-filter* reconstruction (§7.4 applies the loop filters only after the tile
//! is decoded), so the encoder applies them once to its final reconstruction to match a conformant
//! decoder's output.
//!
//! Deblock: under `TX_MODE_SELECT` a luma block (≤32×32) uses one block-size or several smaller
//! square transforms; 4:4:4 chroma always uses the block-size transform. `filterSize` is
//! `Min(16 luma / 8 chroma, baseSize)` where `baseSize` is the
//! minimum transform width across the edge: 4 (narrow, §7.14.6.3) for any edge touching a 4×4
//! transform, 8 (wide, §7.14.6.4 `log2Size = 3`) for an 8↔8 edge, and 16 (wide, §7.14.6.4
//! `log2Size = 4`) for a 16↔16 luma edge. The frame is a single intra key-frame with one segment,
//! `loop_filter_sharpness = 0`, and `loop_filter_delta_enabled = 0`; `delta_lf_present = 1`, so the
//! level is the frame `loop_filter_level` plus a per-superblock `DeltaLF` (each edge takes the level
//! of its q0-side block, §7.14.4 — the same `loop_filter_level` for both luma passes and chroma).
//!
//! CDEF: a single signaled strength set (`cdef_bits = 0`) is applied to every 8×8 block; the
//! per-block `cdef_idx` is `L(0)` (no tile bits). For 4:4:4, chroma shares the luma grid and
//! `Cdef_Uv_Dir` is the identity.
//!
//! **Choosing the filters.** Every parameter is decided against the source, by the distortion the
//! filter leaves, in the decoder's order — each stage is chosen on the output of the stages before
//! it: the frame `loop_filter_level` ([`choose_deblock_level`]), then the luma and the chroma CDEF
//! strength pairs ([`choose_cdef`]), then, per 256×256 loop-restoration unit, whether the default
//! Wiener filter is applied at all ([`choose_lr_units`]). Each stage's candidates include
//! switching the filter off, so the decoded output is never further from the source than the
//! reconstruction before the in-loop filters. The level and the strengths are frame-header fields
//! of fixed width, so their rate does not depend on the choice and distortion alone ranks them; a
//! restoration unit's few signalling bits are left out of its decision.

use gamut_color::ChromaSubsampling;

use crate::geom::PlaneGeom;
use crate::tile::chroma_tx_log2;

/// `MAX_LOOP_FILTER` (§3): the largest `loop_filter_level`.
const MAX_LOOP_FILTER: u8 = 63;

/// The in-loop filter parameters a lossy frame signals in its header and a decoder applies to the
/// whole frame: the deblocking level and the CDEF strength set. Loop restoration is decided per
/// unit, on the possibly upscaled frame, and travels separately as [`LrUnits`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameFilters {
    /// `loop_filter_level[0..4]` (§5.9.11): one level for both luma passes and both chroma
    /// planes. `0` switches the loop filter off for the whole frame (§7.4 step 1).
    pub(crate) deblock_level: u8,
    /// The single CDEF strength set (`cdef_bits = 0`).
    pub(crate) cdef: CdefStrengths,
}

/// The deblocking levels [`choose_deblock_level`] sweeps after `0` (the filter off): every eighth
/// level, and the largest. The search then refines around the best of these.
const DEBLOCK_COARSE: [u8; 8] = [8, 16, 24, 32, 40, 48, 56, MAX_LOOP_FILTER];

/// Chooses the frame `loop_filter_level` by the distortion it leaves: `sse(level)` is the squared
/// error against the source of the reconstruction deblocked at `level`.
///
/// A coarse sweep (`0`, the filter off, then [`DEBLOCK_COARSE`]) is followed by a
/// step-halving refinement (4, 2, 1) around the best level so far, so about fifteen levels are
/// tried rather than all 64. The lowest level wins a tie. The level is a fixed-width header field,
/// so its rate is the same whatever is chosen.
pub(crate) fn choose_deblock_level(mut sse: impl FnMut(u8) -> u64) -> u8 {
    // Each level is measured once: the refinement revisits levels the sweep already tried.
    let mut tried = [None; MAX_LOOP_FILTER as usize + 1];
    let mut cost = |level: u8| -> (u64, u8) {
        let sse = *tried[usize::from(level)].get_or_insert_with(|| sse(level));
        (sse, level)
    };
    let mut best = cost(0);
    for level in DEBLOCK_COARSE {
        best = best.min(cost(level));
    }
    for step in [4u8, 2, 1] {
        let centre = best.1;
        let below = cost(centre.saturating_sub(step));
        let above = cost(centre.saturating_add(step).min(MAX_LOOP_FILTER));
        best = best.min(below).min(above);
    }
    best.1
}

/// The per-edge loop-filter strength thresholds (§7.14.4), all derived from `loop_filter_level` at
/// `loop_filter_sharpness = 0`: `limit`/`blimit` gate whether filtering happens, `thresh` the
/// high-edge-variance test.
#[derive(Clone, Copy)]
struct Strength {
    limit: i32,
    blimit: i32,
    thresh: i32,
    /// The frame's bit depth. The three thresholds above are already scaled to it (§7.14.6.2
    /// applies `<< (BitDepth - 8)` at each comparison); this carries the depth on to the flatness
    /// thresholds and the centring [`narrow_apply`] does, so one value travels with the edge
    /// instead of a second parameter alongside it.
    bit_depth: u32,
}

/// Computes the deblock masks (§7.14.6.2) for one boundary sample and, if the filter applies,
/// returns `(hevMask, flatMask)`. `base` is the index of `q0`; `step` the stride perpendicular to
/// the edge (`1` vertical, row stride horizontal). `filter_size` is 4 or 8; `is_luma` selects the
/// tap count. Returns `None` when `filterMask == 0` (no filtering).
fn deblock_masks(
    buf: &[u16],
    base: usize,
    step: usize,
    filter_size: usize,
    is_luma: bool,
    st: Strength,
) -> Option<(bool, bool, bool)> {
    let Strength {
        limit,
        blimit,
        thresh,
        bit_depth,
    } = st;
    let s = |k: isize| -> i32 { i32::from(buf[(base as isize + k * step as isize) as usize]) };
    let (q0, q1, p0, p1) = (s(0), s(1), s(-1), s(-2));

    // filterLen: 4 for the narrow case; chroma wide = 6; luma wide (filterSize 8) = 8. Samples
    // beyond ±2 are only read (and only in bounds) when the corresponding filterLen requires them.
    let filter_len = if filter_size == 4 {
        4
    } else if !is_luma {
        6
    } else {
        8
    };

    let hev = (p1 - p0).abs() > thresh || (q1 - q0).abs() > thresh;

    let mut mask = (p1 - p0).abs() > limit
        || (q1 - q0).abs() > limit
        || (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > blimit;
    let (q2, p2) = if filter_len >= 6 {
        let (q2, p2) = (s(2), s(-3));
        mask |= (p2 - p1).abs() > limit || (q2 - q1).abs() > limit;
        (q2, p2)
    } else {
        (0, 0)
    };
    let (q3, p3) = if filter_len >= 8 {
        let (q3, p3) = (s(3), s(-4));
        mask |= (p3 - p2).abs() > limit || (q3 - q2).abs() > limit;
        (q3, p3)
    } else {
        (0, 0)
    };
    if mask {
        return None; // filterMask == 0
    }

    // flatMask (only meaningful for filterSize >= 8, where filterLen >= 6 so p2/q2 are read);
    // thresholdBd = 1 << (BitDepth - 8).
    let threshold_bd = 1 << (bit_depth - 8);
    let flat = if filter_size >= 8 {
        let t = threshold_bd;
        let mut fm = (p1 - p0).abs() > t
            || (q1 - q0).abs() > t
            || (p2 - p0).abs() > t
            || (q2 - q0).abs() > t;
        if filter_len >= 8 {
            fm |= (p3 - p0).abs() > t || (q3 - q0).abs() > t;
        }
        !fm
    } else {
        false
    };

    // flatMask2 (only required for filterSize == 16): are the outer samples p4..p6 / q4..q6 also flat?
    // Only read at the wide 16-tap luma edges, where p6 = s(-7) / q6 = s(6) are guaranteed in bounds.
    let flat2 = if filter_size >= 16 {
        let t = threshold_bd;
        let (q4, q5, q6, p4, p5, p6) = (s(4), s(5), s(6), s(-5), s(-6), s(-7));
        let fm = (p6 - p0).abs() > t
            || (q6 - q0).abs() > t
            || (p5 - p0).abs() > t
            || (q5 - q0).abs() > t
            || (p4 - p0).abs() > t
            || (q4 - q0).abs() > t;
        !fm
    } else {
        false
    };
    Some((hev, flat, flat2))
}

/// Narrow (4-tap) deblock filter (§7.14.6.3): modifies up to two samples each side of the edge.
///
/// The samples are centred by `0x80 << (BitDepth - 8)` — mid-grey — so the intermediates live in
/// `-(1 << (BitDepth-1)) ..= (1 << (BitDepth-1)) - 1`, which is exactly the range `filter4_clamp`
/// confines them to.
fn narrow_apply(buf: &mut [u16], base: usize, step: usize, hev: bool, bit_depth: u32) {
    let q0 = i32::from(buf[base]);
    let q1 = i32::from(buf[base + step]);
    let p0 = i32::from(buf[base - step]);
    let p1 = i32::from(buf[base - 2 * step]);
    let mid = 1i32 << (bit_depth - 1);
    let clamp = |v: i32| v.clamp(-mid, mid - 1);
    let (ps1, ps0, qs0, qs1) = (p1 - mid, p0 - mid, q0 - mid, q1 - mid);
    let mut filter = if hev { clamp(ps1 - qs1) } else { 0 };
    filter = clamp(filter + 3 * (qs0 - ps0));
    let filter1 = clamp(filter + 4) >> 3;
    let filter2 = clamp(filter + 3) >> 3;
    buf[base] = (clamp(qs0 - filter1) + mid) as u16; // oq0
    buf[base - step] = (clamp(ps0 + filter2) + mid) as u16; // op0
    if !hev {
        let f = (filter1 + 1) >> 1; // Round2(filter1, 1)
        buf[base + step] = (clamp(qs1 - f) + mid) as u16; // oq1
        buf[base - 2 * step] = (clamp(ps1 + f) + mid) as u16; // op1
    }
}

/// The clamped source offset for a wide deblock tap: row `i + j` confined to `[lo, hi]`. The filter
/// is symmetric in `j` with symmetric taps, so reading `i + j` vs `i - j` sums to the same value —
/// that `+`→`-` flip is equivalent.
fn wide_tap_pos(i: isize, j: isize, lo: isize, hi: isize) -> isize {
    (i + j).clamp(lo, hi)
}

/// Wide (low-pass) deblock filter (§7.14.6.4), parameterized by `log2_size`. `log2_size == 4` (a
/// 16↔16 luma edge) uses `n = 6, n2 = 1` (modifies 6 samples each side from p6..q6); `log2_size == 3`
/// uses `n = 3, n2 = 0` for luma (3 each side) and `n = 2, n2 = 1` for chroma (2 each side). All taps
/// read the original samples, then write.
fn wide_apply(buf: &mut [u16], base: usize, step: usize, is_luma: bool, log2_size: u32) {
    let (n, n2): (isize, isize) = if log2_size == 4 {
        (6, 1)
    } else if is_luma {
        (3, 0)
    } else {
        (2, 1)
    };
    let read = |k: isize| -> i32 { i32::from(buf[(base as isize + k * step as isize) as usize]) };
    let mut out = [0i32; 12]; // up to 2*n = 12 modified samples (i = -n .. n-1)
    let rnd = 1i32 << (log2_size - 1);
    for i in -n..n {
        let mut t = 0i32;
        for j in -n..=n {
            let p = wide_tap_pos(i, j, -(n + 1), n);
            let tap = if j.abs() <= n2 { 2 } else { 1 };
            t += read(p) * tap;
        }
        out[(i + n) as usize] = (t + rnd) >> log2_size; // Round2(t, log2Size)
    }
    for i in -n..n {
        buf[(base as isize + i * step as isize) as usize] = out[(i + n) as usize] as u16;
    }
}

/// Filters one boundary sample: computes the masks, then applies the filter per the §7.14.6.1
/// selection — narrow if `filterSize == 4` or `!flatMask`; else wide `log2Size == 3` if
/// `filterSize == 8` or `!flatMask2`; else (a 16↔16 luma edge) wide `log2Size == 4`.
fn filter_sample(
    buf: &mut [u16],
    base: usize,
    step: usize,
    filter_size: usize,
    is_luma: bool,
    st: Strength,
) {
    if let Some((hev, flat, flat2)) = deblock_masks(buf, base, step, filter_size, is_luma, st) {
        if filter_size == 4 || !flat {
            narrow_apply(buf, base, step, hev, st.bit_depth);
        } else if filter_size == 8 || !flat2 {
            wide_apply(buf, base, step, is_luma, 3);
        } else {
            wide_apply(buf, base, step, is_luma, 4);
        }
    }
}

/// Applies the deblocking loop filter to the coded-grid reconstruction planes in place (§7.14).
/// `geom` supplies each plane's row stride and its visible extent; `mi_cols` strides the per-MI
/// maps, which stay on the **luma** grid, so a chroma coordinate is scaled back to it by
/// [`PlaneGeom::mi_col`]/[`PlaneGeom::mi_row`]. Per §7.14.1 each plane walks its own sample grid —
/// which is what makes chroma step `1 << subsampling` luma samples per edge — while the `onScreen`
/// test stays in luma coordinates. The luma transform size per 4×4 MI cell comes from
/// `tx_log2` (which, under `TX_MODE_SELECT`, can be smaller than the block); 4:4:4 chroma always uses
/// the block-size transform, so its size is `mi_bsl + 2`. Only transform-block edges are filtered; the
/// filter size is `Min(Tx_Width)` across the edge (capped at 16 luma / 8 chroma) — 4 (narrow) for any
/// edge touching a 4×4 transform, 8 (wide) for an 8↔8 edge, 16 (wide) for a 16↔16 luma edge. Partial
/// bottom/right edges filter into the coded-grid padding exactly as a conformant decoder does (the
/// coded grid is a multiple of 8, so the reads stay in bounds); the padding is then cropped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn deblock(
    planes: &mut [Vec<u16>; 3],
    geom: &[PlaneGeom; 3],
    subsampling: ChromaSubsampling,
    mi_cols: usize,
    tx_log2: &[u8],
    tx_log2_h: &[u8],
    mi_bsl: &[u8],
    mi_bsl_h: &[u8],
    mi_dlf: &[i8],
    level: u8,
    bit_depth: u32,
) {
    let base_lvl = i32::from(level);
    if base_lvl == 0 {
        // §7.4 step 1: with `loop_filter_level[0]` and `[1]` both 0 the loop filter process is not
        // invoked at all, whatever `DeltaLF` a superblock carries.
        return;
    }
    // The per-superblock loop-filter level is `Clip3(0, 63, loop_filter_level + DeltaLF)`; with
    // `loop_filter_sharpness = 0` the strength thresholds derive from it directly (§7.14.4/.5).
    // §7.14.6.2 compares against `limit << (BitDepth - 8)` and friends; the three thresholds are
    // derived from the 8-bit level first and then scaled, so the derivation itself stays the
    // spec's.
    let shift = bit_depth - 8;
    let strength_for = |cell: usize| -> Strength {
        let lvl = (base_lvl + i32::from(mi_dlf[cell])).clamp(0, 63);
        let limit = lvl.max(1);
        Strength {
            limit: limit << shift,
            blimit: (2 * (lvl + 2) + limit) << shift,
            thresh: (lvl >> 4) << shift,
            bit_depth,
        }
    };

    for (plane_idx, plane) in planes.iter_mut().enumerate() {
        let is_luma = plane_idx == 0;
        let g = geom[plane_idx];
        let (coded_w, width, height) = (g.coded_w, g.w, g.h);
        let size_cap = if is_luma { 16 } else { 8 };
        // Transform width/height (log2) for this plane at MI `cell`: luma uses the signaled tx size;
        // 4:4:4 chroma uses the block-size transform (`mi_bsl + 2`) capped at TX_32X32 (chroma never
        // uses TX_64X64, so a 64×64 block's chroma is a raster of 32×32 transforms with an edge at 32).
        // Under subsampling the chroma transform follows the plane residual size (§5.11.38), which
        // is not `mi_bsl + 2`: a 4:2:0 16x16 block has an 8x8 chroma transform, not 16x16. The
        // block size is recovered from the per-MI log2 maps and run through §5.11.37. At 4:4:4 this
        // reduces to the old `(mi_bsl + 2).min(5)`.
        let chroma_log2 = |cell: usize| -> (u32, u32) {
            let bw = 1usize << (u32::from(mi_bsl[cell]) + 2);
            let bh = 1usize << (u32::from(mi_bsl_h[cell]) + 2);
            // A block with no valid chroma residual is a conformance violation the partition search
            // must prevent (§6.10.4); it cannot reach the filter, which runs after coding.
            chroma_tx_log2(bw, bh, subsampling).unwrap_or((2, 2))
        };
        let txlog2 = |cell: usize| -> u32 {
            if is_luma {
                u32::from(tx_log2[cell])
            } else {
                chroma_log2(cell).0
            }
        };
        // The transform *height* (log2) — equals `txlog2` for square transforms, differs for
        // rectangular ones; the horizontal pass sizes its edges by this.
        let txlog2_h = |cell: usize| -> u32 {
            if is_luma {
                u32::from(tx_log2_h[cell])
            } else {
                chroma_log2(cell).1
            }
        };

        // Pass 0: vertical edges. Each on-screen edge (column x = 4, 8, … inside the frame) filters
        // the four sample rows of its MI block, when the column is a transform-block edge. The whole
        // pass must finish before the horizontal pass, which reads the vertical-filtered samples.
        let mut x = 4;
        while x < width {
            let mut y = 0;
            while y < height {
                let (col, row) = g.deblock_mi(x, y);
                let txw = 1usize << txlog2(row * mi_cols + col);
                if x % txw == 0 {
                    // §7.14.2 steps the neighbour by `dx << subX`, not by one MI cell. At 4:2:0 and
                    // 4:2:2 consecutive chroma edges land on MI cols 1, 3, 5 …, so `col - 1` is the
                    // *even* cell of the current edge's own group — normally the same block — and
                    // `Min(prevTxSz, txSz)` would silently collapse to `txSz`. Identity at 4:4:4.
                    let prev_txw = 1usize << txlog2(row * mi_cols + (col - (1 << g.ss_x)));
                    let filter_size = prev_txw.min(txw).min(size_cap);
                    // The edge takes the level of its q0-side (right) block (§7.14.4).
                    let st = strength_for(row * mi_cols + col);
                    for i in 0..4 {
                        filter_sample(plane, (y + i) * coded_w + x, 1, filter_size, is_luma, st);
                    }
                }
                y += 4;
            }
            x += 4;
        }
        // Pass 1: horizontal edges (row y = 4, 8, … inside the frame), filtering the four columns of
        // each MI block when the row is a transform-block edge.
        let mut y = 4;
        while y < height {
            let mut x = 0;
            while x < width {
                let (col, row) = g.deblock_mi(x, y);
                let txh = 1usize << txlog2_h(row * mi_cols + col);
                if y % txh == 0 {
                    // §7.14.2's `prevRow = row - (dy << subY)`; see the vertical pass above.
                    let prev_txh = 1usize << txlog2_h((row - (1 << g.ss_y)) * mi_cols + col);
                    let filter_size = prev_txh.min(txh).min(size_cap);
                    // The edge takes the level of its q0-side (bottom) block (§7.14.4).
                    let st = strength_for(row * mi_cols + col);
                    for i in 0..4 {
                        filter_sample(
                            plane,
                            y * coded_w + (x + i),
                            coded_w,
                            filter_size,
                            is_luma,
                            st,
                        );
                    }
                }
                x += 4;
            }
            y += 4;
        }
    }
}

// ===== CDEF (Constrained Directional Enhancement Filter, §7.15) =====

/// `Cdef_Uv_Dir[subsampling_x][subsampling_y][8]` (§7.15.2): the chroma direction for a given luma
/// direction.
///
/// The identity for 4:4:4 (`[0][0]`) and 4:2:0 (`[1][1]`), which is why chroma could take the luma
/// direction unchanged until 4:2:2 arrived. The 4:2:2 row `[1][0]` is **not a permutation** — 2
/// appears once, 6 three times, and 1 and 3 not at all — so it cannot be expressed as a rotation or
/// an offset; it is a literal lookup. `[0][1]` is the 4:4:0 layout, which AV1 cannot signal.
static CDEF_UV_DIR: [[[usize; 8]; 2]; 2] = [
    [[0, 1, 2, 3, 4, 5, 6, 7], [1, 2, 2, 2, 3, 4, 6, 0]],
    [[7, 0, 2, 4, 5, 6, 6, 6], [0, 1, 2, 3, 4, 5, 6, 7]],
];

/// `Cdef_Directions[8][2][2]` (§7.15.3): the (row, col) sample offsets for each of the 8 directions.
static CDEF_DIRECTIONS: [[[i32; 2]; 2]; 8] = [
    [[-1, 1], [-2, 2]],
    [[0, 1], [-1, 2]],
    [[0, 1], [0, 2]],
    [[0, 1], [1, 2]],
    [[1, 1], [2, 2]],
    [[1, 0], [2, 1]],
    [[1, 0], [2, 0]],
    [[1, 0], [2, -1]],
];
/// `Cdef_Pri_Taps[2][2]` / `Cdef_Sec_Taps[2][2]` (§7.15.3), selected by `priStr & 1`.
static CDEF_PRI_TAPS: [[i32; 2]; 2] = [[4, 2], [3, 3]];
static CDEF_SEC_TAPS: [[i32; 2]; 2] = [[2, 1], [2, 1]];
/// `Div_Table[9]` (§7.15.2).
static CDEF_DIV_TABLE: [i64; 9] = [0, 840, 420, 280, 210, 168, 140, 120, 105];

/// `FloorLog2(x)` for `x >= 1`.
fn floor_log2(x: i32) -> i32 {
    31 - (x as u32).leading_zeros() as i32
}

/// `constrain(diff, threshold, damping)` (§7.15.3): the soft-threshold applied to each neighbour
/// difference before it is weighted into the filter sum.
fn constrain(diff: i32, threshold: i32, damping: i32) -> i32 {
    if threshold == 0 {
        return 0;
    }
    let damping_adj = (damping - floor_log2(threshold)).max(0);
    let sign = if diff < 0 { -1 } else { 1 };
    sign * (threshold - (diff.abs() >> damping_adj)).clamp(0, diff.abs())
}

/// The CDEF strength set a frame signals (`cdef_bits = 0`, so one set for every block), in the
/// 8-bit domain and as stored after the §5.9.19 `3 → 4` secondary mapping: a primary strength in
/// `0..=15` and a secondary one in `{0, 1, 2, 4}`, for luma and for chroma. All zero makes CDEF a
/// no-op. `CdefDamping` is fixed at 3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CdefStrengths {
    /// `cdef_y_pri_strength[0]`.
    pub(crate) y_pri: u8,
    /// `cdef_y_sec_strength[0]`, after the `3 → 4` mapping.
    pub(crate) y_sec: u8,
    /// `cdef_uv_pri_strength[0]`.
    pub(crate) uv_pri: u8,
    /// `cdef_uv_sec_strength[0]`, after the `3 → 4` mapping.
    pub(crate) uv_sec: u8,
}

/// The secondary strengths a frame can signal, after the §5.9.19 `3 → 4` mapping.
const CDEF_SEC_STRENGTHS: [u8; 4] = [0, 1, 2, 4];

/// Chooses the CDEF strength set by the distortion it leaves. `sse(pri, sec)` filters the
/// deblocked frame with that pair as both the luma and the chroma strengths and returns the squared
/// error against the source of the luma plane and of the chroma planes, separately.
///
/// Every pair `0..=15 × {0, 1, 2, 4}` is tried. Luma CDEF reads and writes luma only and chroma
/// CDEF chroma only — the chroma direction is taken from luma's direction search, which no strength
/// moves — so the luma pair and the chroma pair are each chosen on their own planes' error from
/// the same sweep. The weakest pair wins a tie, `(0, 0)` (off) before all.
pub(crate) fn choose_cdef(mut sse: impl FnMut(u8, u8) -> (u64, u64)) -> CdefStrengths {
    let mut luma = (u64::MAX, 0u8, 0u8);
    let mut chroma = (u64::MAX, 0u8, 0u8);
    for pri in 0..=15u8 {
        for sec in CDEF_SEC_STRENGTHS {
            let (y, uv) = sse(pri, sec);
            luma = luma.min((y, pri, sec));
            chroma = chroma.min((uv, pri, sec));
        }
    }
    CdefStrengths {
        y_pri: luma.1,
        y_sec: luma.2,
        uv_pri: chroma.1,
        uv_sec: chroma.2,
    }
}

/// `CdefDamping` (= `cdef_damping_minus_3 + 3`); fixed at 3 here.
/// `FILTER_BITS` (§3): the precision of the interpolation and restoration filter taps, so a unit
/// DC gain is `1 << FILTER_BITS`.
const FILTER_BITS: u32 = 7;

pub(crate) const CDEF_DAMPING: i32 = 3;

/// CDEF direction process (§7.15.2): finds the dominant direction `yDir` (0..7) and the variance
/// `var` of the luma 8×8 block whose top-left is at coded `(x0, y0)`. The index-based loops mirror
/// the spec's `partial`/`cost` accumulation directly.
/// §7.15.2: the eight directional `partial` accumulators for the 8×8 luma block at `(x0, y0)`. Each
/// partial enters the direction cost only as a square (`cost += partial²`), so negating an entire
/// accumulator — a `+=`→`-=` flip on any one line below — leaves every cost unchanged and is therefore
/// equivalent; the live arithmetic is the index math and the `- 128` centring.
/// A sample brought into the 8-bit domain and centred, for the §7.15.2 direction search:
/// `(x >> (BitDepth - 8)) - 128`.
///
/// The `- 128` is inert — each direction cost is a sum of squared `partial` sums, and the chosen
/// direction and variance are an argmax and a cost *difference*, both unchanged by a uniform offset
/// of every sample — so mutating it is equivalent. The **shift** is not: `var` scales with the
/// square of the sample range and reaches the filter through `FloorLog2(var >> 6)`, so a
/// high-bit-depth block that skipped it would pick a different primary strength.
fn centred(sample: u16, coeff_shift: u32) -> i64 {
    i64::from(sample >> coeff_shift) - 128
}

#[allow(clippy::needless_range_loop)]
fn cdef_partials(
    luma: &[u16],
    coded_w: usize,
    x0: usize,
    y0: usize,
    coeff_shift: u32,
) -> [[i64; 15]; 8] {
    let mut partial = [[0i64; 15]; 8];
    for i in 0..8 {
        for j in 0..8 {
            let x = centred(luma[(y0 + i) * coded_w + (x0 + j)], coeff_shift);
            partial[0][i + j] += x;
            partial[1][i + j / 2] += x;
            partial[2][i] += x;
            partial[3][3 + i - j / 2] += x;
            partial[4][7 + i - j] += x;
            partial[5][3 - i / 2 + j] += x;
            partial[6][j] += x;
            partial[7][i / 2 + j] += x;
        }
    }
    partial
}

#[allow(clippy::needless_range_loop)]
fn cdef_direction(
    luma: &[u16],
    coded_w: usize,
    x0: usize,
    y0: usize,
    coeff_shift: u32,
) -> (usize, i32) {
    let partial = cdef_partials(luma, coded_w, x0, y0, coeff_shift);
    let mut cost = [0i64; 8];
    for i in 0..8 {
        cost[2] += partial[2][i] * partial[2][i];
        cost[6] += partial[6][i] * partial[6][i];
    }
    cost[2] *= CDEF_DIV_TABLE[8];
    cost[6] *= CDEF_DIV_TABLE[8];
    for i in 0..7 {
        cost[0] += (partial[0][i] * partial[0][i] + partial[0][14 - i] * partial[0][14 - i])
            * CDEF_DIV_TABLE[i + 1];
        cost[4] += (partial[4][i] * partial[4][i] + partial[4][14 - i] * partial[4][14 - i])
            * CDEF_DIV_TABLE[i + 1];
    }
    cost[0] += partial[0][7] * partial[0][7] * CDEF_DIV_TABLE[8];
    cost[4] += partial[4][7] * partial[4][7] * CDEF_DIV_TABLE[8];
    let mut i = 1;
    while i < 8 {
        for j in 0..5 {
            cost[i] += partial[i][3 + j] * partial[i][3 + j];
        }
        cost[i] *= CDEF_DIV_TABLE[8];
        for j in 0..3 {
            cost[i] += (partial[i][j] * partial[i][j] + partial[i][10 - j] * partial[i][10 - j])
                * CDEF_DIV_TABLE[2 * j + 2];
        }
        i += 2;
    }
    let mut best_cost = 0i64;
    let mut y_dir = 0usize;
    for (d, &c) in cost.iter().enumerate() {
        if c > best_cost {
            best_cost = c;
            y_dir = d;
        }
    }
    let var = ((best_cost - cost[(y_dir + 4) & 7]) >> 10) as i32;
    (y_dir, var)
}

/// A secondary CDEF tap direction: `±2` off the primary direction, mod 8 (§7.15.3). The two offsets
/// `±2` are summed symmetrically into the filter, so swapping `+`/`-` here selects the same pair and
/// is equivalent.
fn sec_dir(dir: usize, off: i32) -> usize {
    ((dir as i32 + off) & 7) as usize
}

/// CDEF filter process (§7.15.3) for one 8×8 (or sub-sampled) block of `plane`. Reads the
/// (deblocked) `input` and writes the filtered samples into `output`; a sample read outside the coded
/// grid is unavailable and skipped. `w`/`h` are the block extent in this plane.
#[allow(clippy::too_many_arguments)]
fn cdef_filter_block(
    input: &[u16],
    output: &mut [u16],
    coded_w: usize,
    coded_h: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    pri_str: i32,
    sec_str: i32,
    damping: i32,
    dir: usize,
    coeff_shift: u32,
) {
    let at = |y: i32, x: i32| -> Option<i32> {
        if y >= 0 && (y as usize) < coded_h && x >= 0 && (x as usize) < coded_w {
            Some(i32::from(input[y as usize * coded_w + x as usize]))
        } else {
            None
        }
    };
    // §7.15.3 indexes the tap tables by `(priStr >> coeffShift) & 1` — the parity of the *signaled*
    // strength, before it is scaled to the frame's depth.
    let taps_idx = ((pri_str >> coeff_shift) & 1) as usize;
    let pri_taps = &CDEF_PRI_TAPS[taps_idx];
    let sec_taps = &CDEF_SEC_TAPS[taps_idx];
    for i in 0..h {
        for j in 0..w {
            let x = i32::from(input[(y0 + i) * coded_w + (x0 + j)]);
            let (mut sum, mut mn, mut mx) = (0i32, x, x);
            for k in 0..2 {
                for &sign in &[-1i32, 1] {
                    let pr = at(
                        y0 as i32 + i as i32 + sign * CDEF_DIRECTIONS[dir][k][0],
                        x0 as i32 + j as i32 + sign * CDEF_DIRECTIONS[dir][k][1],
                    );
                    if let Some(p) = pr {
                        sum += pri_taps[k] * constrain(p - x, pri_str, damping);
                        mx = mx.max(p);
                        mn = mn.min(p);
                    }
                    for dir_off in [-2i32, 2] {
                        let sd = sec_dir(dir, dir_off);
                        let s = at(
                            y0 as i32 + i as i32 + sign * CDEF_DIRECTIONS[sd][k][0],
                            x0 as i32 + j as i32 + sign * CDEF_DIRECTIONS[sd][k][1],
                        );
                        if let Some(s) = s {
                            sum += sec_taps[k] * constrain(s - x, sec_str, damping);
                            mx = mx.max(s);
                            mn = mn.min(s);
                        }
                    }
                }
            }
            // `(8 + sum - (sum < 0)) >> 4` is `Round2Signed(sum, 4)` (round to nearest, ties toward 0).
            let v = x + gamut_dsp::math::round2_signed(sum, 4);
            output[(y0 + i) * coded_w + (x0 + j)] = v.clamp(mn, mx) as u16;
        }
    }
}

/// Whether all four CDEF strengths are zero, in which case the filter is a no-op and the loop is
/// skipped. This is a pure optimization: [`cdef_filter_block`] with zero primary and secondary
/// strength leaves every sample unchanged (each neighbour contributes `constrain(_, 0, _) == 0`, and
/// the centre is clamped to a range that already contains it), so taking or skipping the early return
/// produces identical output — every mutation of this predicate is therefore equivalent.
fn cdef_disabled(y_pri: i32, y_sec: i32, uv_pri: i32, uv_sec: i32) -> bool {
    y_pri == 0 && y_sec == 0 && uv_pri == 0 && uv_sec == 0
}

/// §7.15.1: an 8×8 CDEF block is skipped when all four of its 4×4 MI cells are `skip`. `r4`/`c4` are
/// the top-left cell; the other three are the right, below, and below-right neighbours.
fn block_all_skip(mi_skip: &[u8], mi_cols: usize, r4: usize, c4: usize) -> bool {
    mi_skip[r4 * mi_cols + c4] != 0
        && mi_skip[r4 * mi_cols + (c4 + 1)] != 0
        && mi_skip[(r4 + 1) * mi_cols + c4] != 0
        && mi_skip[(r4 + 1) * mi_cols + (c4 + 1)] != 0
}

/// The §7.15.2 direction search's result for every 8×8 luma block of a frame, raster order on the
/// coded grid: `(yDir, var)`, or `None` for a block CDEF skips (§7.15.1, all four 4×4 cells
/// `skip`). Neither depends on the strengths, so [`choose_cdef`]'s sweep searches each block once
/// rather than once per candidate.
pub(crate) struct CdefBlocks(Vec<Option<(usize, i32)>>);

impl CdefBlocks {
    /// Runs the direction search over the deblocked luma `luma` for every 8×8 block CDEF filters.
    pub(crate) fn analyse(
        luma: &[u16],
        geom: &[PlaneGeom; 3],
        mi_skip: &[u8],
        mi_cols: usize,
        bit_depth: u32,
    ) -> Self {
        let (coded_w, coded_h) = (geom[0].coded_w, geom[0].coded_h);
        let blocks = (0..coded_h)
            .step_by(8)
            .flat_map(|y0| (0..coded_w).step_by(8).map(move |x0| (x0, y0)))
            .map(|(x0, y0)| {
                (!block_all_skip(mi_skip, mi_cols, y0 / 4, x0 / 4))
                    .then(|| cdef_direction(luma, coded_w, x0, y0, bit_depth - 8))
            })
            .collect();
        Self(blocks)
    }
}

/// Applies the CDEF deringing filter (§7.15) to the deblocked reconstruction planes, returning the
/// filtered planes. Operates on the coded grid, with one strength set for every 8×8 luma block
/// (`cdef_bits` is 0); `blocks` is the frame's direction search, run on these same `planes` by
/// [`CdefBlocks::analyse`]. All reads are from the pre-CDEF (deblocked) input, so a fresh output
/// is produced. The block position and extent are scaled into each plane by its [`PlaneGeom`], and
/// the chroma direction is the luma one remapped through `Cdef_Uv_Dir`. `num_planes` is AV1's
/// `NumPlanes`: at 1 (monochrome) the chroma pass is skipped entirely, and the signaled
/// `cdef_uv_*` strengths do not exist.
pub(crate) fn cdef(
    planes: &[Vec<u16>; 3],
    geom: &[PlaneGeom; 3],
    blocks: &CdefBlocks,
    strengths: CdefStrengths,
    num_planes: usize,
    bit_depth: u32,
) -> [Vec<u16>; 3] {
    // §7.15.1: every strength is signaled in the 8-bit domain and scaled to the frame's depth here,
    // and the damping grows by the same shift.
    let coeff_shift = bit_depth - 8;
    let scaled = |s: u8| i32::from(s) << coeff_shift;
    let (y_pri, y_sec, uv_pri, uv_sec) = (
        scaled(strengths.y_pri),
        scaled(strengths.y_sec),
        scaled(strengths.uv_pri),
        scaled(strengths.uv_sec),
    );
    let mut out = planes.clone();
    if cdef_disabled(y_pri, y_sec, uv_pri, uv_sec) {
        return out;
    }
    // The sweep is over 8x8 **luma** blocks; each plane's own extent comes from its geometry.
    let (coded_w, coded_h) = (geom[0].coded_w, geom[0].coded_h);
    let mut analysed = blocks.0.iter();
    for y0 in (0..coded_h).step_by(8) {
        for x0 in (0..coded_w).step_by(8) {
            // §7.15.1: CDEF is not applied to an 8×8 block whose four 4×4 cells are all `skip`.
            let Some(&Some((y_dir, var))) = analysed.next() else {
                continue;
            };
            // Luma: the primary strength is scaled by the block variance (§7.15.1 steps 4-5).
            let dir = if y_pri == 0 { 0 } else { y_dir };
            let var_str = if var >> 6 != 0 {
                floor_log2(var >> 6).min(12)
            } else {
                0
            };
            let luma_pri = if var != 0 {
                (y_pri * (4 + var_str) + 8) >> 4
            } else {
                0
            };
            cdef_filter_block(
                &planes[0],
                &mut out[0],
                coded_w,
                coded_h,
                x0,
                y0,
                8,
                8,
                luma_pri,
                y_sec,
                CDEF_DAMPING + coeff_shift as i32,
                dir,
                coeff_shift,
            );
            // Chroma: no variance scaling, damping reduced by 1, direction remapped through
            // `Cdef_Uv_Dir` (§7.15.2 step 11). Identity at 4:4:4 and 4:2:0; 4:2:2 is the one layout
            // where the chroma direction genuinely differs from the luma one.
            let uv = geom[1];
            let cdir = if uv_pri == 0 {
                0
            } else {
                CDEF_UV_DIR[uv.ss_x as usize][uv.ss_y as usize][y_dir]
            };
            for plane in 1..num_planes {
                let g = geom[plane];
                let (px, py) = g.scale_pos(x0, y0);
                let (pw, ph) = g.scale_extent(8, 8);
                cdef_filter_block(
                    &planes[plane],
                    &mut out[plane],
                    g.coded_w,
                    g.coded_h,
                    px,
                    py,
                    pw,
                    ph,
                    uv_pri,
                    uv_sec,
                    CDEF_DAMPING + coeff_shift as i32 - 1,
                    cdir,
                    coeff_shift,
                );
            }
        }
    }
    out
}

/// The default Wiener restoration half-taps (`Wiener_Taps_Mid`, §9.x) — also the per-unit reference
/// the subexp coefficient deltas are coded against, so coding this filter emits a zero delta.
pub(crate) const WIENER_DEFAULT: [i32; 3] = [3, -7, 15];

/// The §7.17.4 Wiener constants at `bit_depth`: `(InterRound0, InterRound1, offset, limit)`.
///
/// The rounding pair is §7.11.3.2's at `isCompound = 0` — `3` and `11`, except at 12 bits where
/// they become `5` and `9`. `offset` re-centres the intermediate so it can be held as an unsigned
/// value in `0..=limit`, which is what [`loop_restore_wiener_luma`] carries between its two passes.
///
/// Extracted because in place these are nearly unobservable: the `+ offset` the horizontal pass
/// adds is removed again by the vertical pass, so a wrong `offset` cancels itself out except at the
/// clip, and `limit` only bites on content extreme enough to saturate the intermediate. Here every
/// value is pinned directly against the spec.
fn wiener_rounding(bit_depth: u32) -> (u32, u32, i32, i32) {
    let (round0, round1) = if bit_depth == 12 {
        (5u32, 9u32)
    } else {
        (3u32, 11u32)
    };
    // §7.17.4: `intermediate + offset` is what the filter actually carries, so `offset << round0`
    // is folded into the horizontal sum and the clip becomes `0..=limit`.
    let offset = 1i32 << (bit_depth + FILTER_BITS - round0 - 1);
    let limit = (1i32 << (bit_depth + 1 + FILTER_BITS - round0)) - 1;
    (round0, round1, offset, limit)
}

/// Applies the §7.17 Wiener loop-restoration filter to the luma plane in place (the post-CDEF
/// reconstruction `cdef`), reading the **deblocked** (pre-CDEF) reconstruction `deblock` for the
/// loop-restoration stripe boundaries. `fh`/`fv` are the three signaled half-taps for the horizontal
/// and vertical 7-tap symmetric filters.
///
/// The frame is processed in 64-row stripes offset by 8 (the first is 56 rows). Within a stripe the
/// filter reads the post-CDEF samples; the two rows immediately above the stripe top and below the
/// stripe bottom come from the deblocked reconstruction (the saved "loop restoration line"), with the
/// third tap replicating the nearer boundary row. At the frame top/bottom and left/right edges the
/// nearest in-frame sample is replicated (a single restoration unit spans the width).
///
/// The rounding is §7.11.3.2's `InterRound0`/`InterRound1` at `isCompound = 0`: `3` and `11`, except
/// at 12 bits where they become `5` and `9`. §7.17.4's `offset` then re-centres the intermediate so
/// it can be held unsigned, and `limit` bounds it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn loop_restore_wiener_luma(
    cdef: &mut [u16],
    deblock: &[u16],
    coded_w: usize,
    width: usize,
    height: usize,
    fh: [i32; 3],
    fv: [i32; 3],
    bit_depth: u32,
) {
    if width == 0 || height == 0 {
        return;
    }
    let (round0, round1, offset, limit) = wiener_rounding(bit_depth);
    let h_round = 1i32 << (round0 - 1);
    let v_round = 1i32 << (round1 - 1);
    // The vertical taps sum to `1 << FILTER_BITS`, so the per-sample `+ offset` the intermediate
    // carries arrives in the vertical sum scaled by exactly that, and is removed once.
    let v_offset = offset << FILTER_BITS;
    // 7-tap symmetric filters. The horizontal centre excludes the implicit `1 << FILTER_BITS`
    // (added per sample below), the vertical centre includes it (§7.17.4).
    let fh7 = [
        fh[0],
        fh[1],
        fh[2],
        -2 * (fh[0] + fh[1] + fh[2]),
        fh[2],
        fh[1],
        fh[0],
    ];
    let fv7 = [
        fv[0],
        fv[1],
        fv[2],
        (1 << FILTER_BITS) - 2 * (fv[0] + fv[1] + fv[2]),
        fv[2],
        fv[1],
        fv[0],
    ];
    let src_cdef = cdef.to_vec();
    // Horizontal 7-tap filter of one source row → a `width`-long buffer holding
    // `intermediate + offset`, clipped to `0..=limit`.
    let h_row = |buf: &[u16], row: usize| -> Vec<i32> {
        (0..width)
            .map(|x| {
                let base = row * coded_w;
                let mut sum = (offset << round0) + i32::from(buf[base + x]) * (1 << FILTER_BITS);
                for (i, &tap) in fh7.iter().enumerate() {
                    let sx = (x as isize + i as isize - 3).clamp(0, width as isize - 1) as usize;
                    sum += i32::from(buf[base + sx]) * tap;
                }
                ((sum + h_round) >> round0).clamp(0, limit)
            })
            .collect()
    };
    let mut st = 0usize;
    while st < height {
        let stripe_h = if st == 0 {
            56.min(height)
        } else {
            64.min(height - st)
        };
        let se = st + stripe_h;
        // Horizontal-filter the band of rows the vertical 7-tap spans: `st-3 ..= se+2`, each picking
        // its source (post-CDEF interior, deblocked boundary, or replicated edge).
        let band: Vec<Vec<i32>> = ((st as isize - 3)..=(se as isize + 2))
            .map(|sr| {
                if sr >= st as isize && sr < se as isize {
                    h_row(&src_cdef, sr as usize)
                } else if sr >= se as isize {
                    // Above the stripe: a deblocked boundary row, replicated past the last stripe.
                    if se < height {
                        h_row(deblock, sr.min(se as isize + 1) as usize)
                    } else {
                        h_row(&src_cdef, height - 1)
                    }
                } else if st > 0 {
                    // Below the stripe: the saved deblocked boundary row.
                    h_row(deblock, sr.max(st as isize - 2) as usize)
                } else {
                    h_row(&src_cdef, 0)
                }
            })
            .collect();
        for y in st..se {
            for x in 0..width {
                let mut sum = -v_offset;
                for (t, &tap) in fv7.iter().enumerate() {
                    let bi = (y as isize + t as isize - 3 - (st as isize - 3)) as usize;
                    sum += band[bi][x] * tap;
                }
                cdef[y * coded_w + x] =
                    gamut_color::clip_pixel((sum + v_round) >> round1, bit_depth);
            }
        }
        st = se;
    }
}

/// The luma `LoopRestorationSize` (§5.9.20): `lr_unit_shift = 2` makes the units 256×256.
pub(crate) const LR_UNIT: usize = 256;

/// A frame's luma loop-restoration decision: for each restoration unit, raster order, whether the
/// unit is restored with the default Wiener filter (`RESTORE_WIENER`) or left as CDEF produced it
/// (`RESTORE_NONE`). Chroma is never restored. A frame none of whose units is restored signals
/// `FrameRestorationType[0] = RESTORE_NONE` and codes no unit at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LrUnits {
    /// `unitCols` (§5.11.57), counted over `UpscaledWidth`.
    pub(crate) cols: usize,
    /// `unitRows` (§5.11.57), counted over `FrameHeight`.
    pub(crate) rows: usize,
    /// Per unit, `row * cols + col`: whether it is Wiener-restored.
    pub(crate) wiener: Vec<bool>,
}

impl LrUnits {
    /// Whether any unit is restored, which is what makes `FrameRestorationType[0]`
    /// `RESTORE_WIENER` rather than `RESTORE_NONE`.
    pub(crate) fn any(&self) -> bool {
        self.wiener.contains(&true)
    }

    /// The index of the unit that restores luma sample `(x, y)`: §7.17.1 takes the unit of the
    /// sample's 4×4 block, with rows offset upwards by 8 — the stripe offset — and the last unit
    /// in each direction absorbing what lies past it.
    fn unit_of(&self, x: usize, y: usize) -> usize {
        let row = ((y / 4 * 4 + 8) / LR_UNIT).min(self.rows - 1);
        let col = (x / LR_UNIT).min(self.cols - 1);
        row * self.cols + col
    }
}

/// Decides each luma restoration unit of a `width × height` frame by the distortion it leaves:
/// a unit is restored when the default Wiener filter's output `restored` is closer to `source`,
/// by squared error over the unit, than the unrestored `cdef`. A tie leaves the unit unrestored.
/// `cdef` and `restored` are strided by `stride`; `source` by `width`. Restoring one unit reads
/// no other unit's output, so each is decided on its own.
pub(crate) fn choose_lr_units<S: Copy + Into<i64>>(
    source: &[S],
    cdef: &[u16],
    restored: &[u16],
    stride: usize,
    width: usize,
    height: usize,
) -> LrUnits {
    let (cols, rows) = (
        crate::tile::lr_unit_count(width, LR_UNIT),
        crate::tile::lr_unit_count(height, LR_UNIT),
    );
    let mut units = LrUnits {
        cols,
        rows,
        wiener: vec![false; cols * rows],
    };
    // Per unit, the squared error restoring it removes (negative where it adds error).
    let mut gain = vec![0i64; cols * rows];
    for y in 0..height {
        for x in 0..width {
            let s: i64 = source[y * width + x].into();
            let off = i64::from(cdef[y * stride + x]) - s;
            let on = i64::from(restored[y * stride + x]) - s;
            gain[units.unit_of(x, y)] += off * off - on * on;
        }
    }
    for (wiener, g) in units.wiener.iter_mut().zip(gain) {
        *wiener = g > 0;
    }
    units
}

/// Writes the restored samples of every unit `lr` restores from `restored` into `cdef`, leaving
/// the other units as CDEF produced them: the luma plane a decoder outputs. Both are strided by
/// `stride`; only the visible `width × height` is touched.
pub(crate) fn apply_lr_units(
    lr: &LrUnits,
    cdef: &mut [u16],
    restored: &[u16],
    stride: usize,
    width: usize,
    height: usize,
) {
    for y in 0..height {
        for x in 0..width {
            if lr.wiener[lr.unit_of(x, y)] {
                cdef[y * stride + x] = restored[y * stride + x];
            }
        }
    }
}

/// `Upscale_Filter` (§7.16.3): the 64-phase, 8-tap superres upscaling filter (dav1d's negated form,
/// so the convolution is `(-(Σ tap·sample) + 64) >> 7`). Phase is `subpelX >> 8` (the top 6 bits).
#[rustfmt::skip]
pub(crate) static RESIZE_FILTER: [[i8; 8]; 64] = [
    [0, 0, 0, -128, 0, 0, 0, 0], [0, 0, 1, -128, -2, 1, 0, 0],
    [0, -1, 3, -127, -4, 2, -1, 0], [0, -1, 4, -127, -6, 3, -1, 0],
    [0, -2, 6, -126, -8, 3, -1, 0], [0, -2, 7, -125, -11, 4, -1, 0],
    [1, -2, 8, -125, -13, 5, -2, 0], [1, -3, 9, -124, -15, 6, -2, 0],
    [1, -3, 10, -123, -18, 6, -2, 1], [1, -3, 11, -122, -20, 7, -3, 1],
    [1, -4, 12, -121, -22, 8, -3, 1], [1, -4, 13, -120, -25, 9, -3, 1],
    [1, -4, 14, -118, -28, 9, -3, 1], [1, -4, 15, -117, -30, 10, -4, 1],
    [1, -5, 16, -116, -32, 11, -4, 1], [1, -5, 16, -114, -35, 12, -4, 1],
    [1, -5, 17, -112, -38, 12, -4, 1], [1, -5, 18, -111, -40, 13, -5, 1],
    [1, -5, 18, -109, -43, 14, -5, 1], [1, -6, 19, -107, -45, 14, -5, 1],
    [1, -6, 19, -105, -48, 15, -5, 1], [1, -6, 19, -103, -51, 16, -5, 1],
    [1, -6, 20, -101, -53, 16, -6, 1], [1, -6, 20, -99, -56, 17, -6, 1],
    [1, -6, 20, -97, -58, 17, -6, 1], [1, -6, 20, -95, -61, 18, -6, 1],
    [2, -7, 20, -93, -64, 18, -6, 2], [2, -7, 20, -91, -66, 19, -6, 1],
    [2, -7, 20, -88, -69, 19, -6, 1], [2, -7, 20, -86, -71, 19, -6, 1],
    [2, -7, 20, -84, -74, 20, -7, 2], [2, -7, 20, -81, -76, 20, -7, 1],
    [2, -7, 20, -79, -79, 20, -7, 2], [1, -7, 20, -76, -81, 20, -7, 2],
    [2, -7, 20, -74, -84, 20, -7, 2], [1, -6, 19, -71, -86, 20, -7, 2],
    [1, -6, 19, -69, -88, 20, -7, 2], [1, -6, 19, -66, -91, 20, -7, 2],
    [2, -6, 18, -64, -93, 20, -7, 2], [1, -6, 18, -61, -95, 20, -6, 1],
    [1, -6, 17, -58, -97, 20, -6, 1], [1, -6, 17, -56, -99, 20, -6, 1],
    [1, -6, 16, -53, -101, 20, -6, 1], [1, -5, 16, -51, -103, 19, -6, 1],
    [1, -5, 15, -48, -105, 19, -6, 1], [1, -5, 14, -45, -107, 19, -6, 1],
    [1, -5, 14, -43, -109, 18, -5, 1], [1, -5, 13, -40, -111, 18, -5, 1],
    [1, -4, 12, -38, -112, 17, -5, 1], [1, -4, 12, -35, -114, 16, -5, 1],
    [1, -4, 11, -32, -116, 16, -5, 1], [1, -4, 10, -30, -117, 15, -4, 1],
    [1, -3, 9, -28, -118, 14, -4, 1], [1, -3, 9, -25, -120, 13, -4, 1],
    [1, -3, 8, -22, -121, 12, -4, 1], [1, -3, 7, -20, -122, 11, -3, 1],
    [1, -2, 6, -18, -123, 10, -3, 1], [0, -2, 6, -15, -124, 9, -3, 1],
    [0, -2, 5, -13, -125, 8, -2, 1], [0, -1, 4, -11, -125, 7, -2, 0],
    [0, -1, 3, -8, -126, 6, -2, 0], [0, -1, 3, -6, -127, 4, -1, 0],
    [0, -1, 2, -4, -127, 3, -1, 0], [0, 0, 1, -2, -128, 1, 0, 0],
];

/// The superres subpel rounding error `out_w·step − (in_w << 14)`. It reaches the offset only as
/// `− err/2`, and [`superres_x0`]'s final `& 0x3fff` discards any multiple of `2¹⁴`: flipping this
/// `−` to `+` shifts `err` by `2·(in_w << 14)`, hence `err/2` by a multiple of `2¹⁴`, so the masked
/// offset is unchanged — that mutation is equivalent. (The `*` and `<<` here are not masked away.)
fn superres_err(out_w: i32, in_w: i32, step: i32) -> i32 {
    out_w * step - (in_w << 14)
}

/// `get_upscale_x0` (§7.16.3): the initial subpel offset for the superres upscale.
fn superres_x0(in_w: usize, out_w: usize, step: i32) -> i32 {
    let (in_w, out_w) = (in_w as i32, out_w as i32);
    let err = superres_err(out_w, in_w, step);
    let x0 = (-((out_w - in_w) << 13) + (out_w >> 1)) / out_w + 128 - (err / 2);
    x0 & 0x3fff
}

/// Upscales one plane horizontally from `src_w` to `dst_w` (§7.16.3 superres). The 8-tap polyphase
/// filter steps the subpel source position by `step = ((src_w << 14) + dst_w/2) / dst_w`, starting at
/// `superres_x0`; out-of-frame samples replicate the nearest column.
pub(crate) fn superres_upscale_plane(
    src: &[u16],
    src_stride: usize,
    frame_w: usize,
    dst_w: usize,
    height: usize,
    bit_depth: u32,
) -> Vec<u16> {
    // The subpel geometry (step/initial offset) is keyed by `FrameWidth`, but the source samples are
    // clamped to the coded grid (`src_stride = 4*bw`, padded past FrameWidth), matching dav1d.
    let step = (((frame_w << 14) + dst_w / 2) / dst_w) as i32;
    let x0 = superres_x0(frame_w, dst_w, step);
    let mut dst = vec![0u16; dst_w * height];
    for y in 0..height {
        let (base, dbase) = (y * src_stride, y * dst_w);
        let mut mx = x0;
        let mut src_x = -1i32;
        for x in 0..dst_w {
            let f = &RESIZE_FILTER[((mx >> 8) & 63) as usize];
            let mut sum = 0i32;
            for (k, &tap) in f.iter().enumerate() {
                let sx = (src_x + k as i32 - 3).clamp(0, src_stride as i32 - 1) as usize;
                sum += i32::from(tap) * i32::from(src[base + sx]);
            }
            dst[dbase + x] = gamut_color::clip_pixel(
                (-sum + (1 << (FILTER_BITS - 1))) >> FILTER_BITS,
                bit_depth,
            );
            mx += step;
            src_x += mx >> 14;
            mx &= 0x3fff;
        }
    }
    dst
}

/// Downscales one plane horizontally from `src_w` to `dst_w` by box-averaging (the encoder's source
/// preparation — its choice, since only the upscale of the coded reconstruction is bit-exact-bound).
pub(crate) fn superres_downscale_plane(
    src: &[u8],
    src_w: usize,
    dst_w: usize,
    height: usize,
) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * height];
    for y in 0..height {
        for x in 0..dst_w {
            let x0 = x * src_w / dst_w;
            // Downscale (`dst_w < src_w`) ⇒ the box `[x0, x1)` always advances by ≥ 1, so no empty-box
            // floor is needed; `.min` keeps the end within the row.
            let x1 = (((x + 1) * src_w) / dst_w).min(src_w);
            let sum: u32 = (x0..x1).map(|sx| u32::from(src[y * src_w + sx])).sum();
            dst[y * dst_w + x] = (sum / (x1 - x0) as u32) as u8;
        }
    }
    dst
}

/// `FrameWidth` (§7.16): the coded (downscaled) width for a superres denominator `denom` (9..=16) and
/// upscaled width `upscaled`.
pub(crate) fn superres_downscaled_width(upscaled: usize, denom: usize) -> usize {
    (upscaled * 8 + denom / 2) / denom
}

#[cfg(test)]
mod tests {

    #[test]
    fn wiener_rounding_matches_the_spec_at_every_depth() {
        // §7.11.3.2 at isCompound = 0 gives (InterRound0, InterRound1) = (3, 11), and only 12-bit
        // moves them to (5, 9). §7.17.4 then derives
        //   offset = 1 << (BitDepth + FILTER_BITS - InterRound0 - 1)
        //   limit  = (1 << (BitDepth + 1 + FILTER_BITS - InterRound0)) - 1
        // Both are nearly inert inside the filter — `offset` cancels between the two passes and
        // `limit` only bites on saturating content — so they are pinned here by value.
        assert_eq!(wiener_rounding(8), (3, 11, 1 << 11, (1 << 13) - 1));
        assert_eq!(wiener_rounding(10), (3, 11, 1 << 13, (1 << 15) - 1));
        assert_eq!(wiener_rounding(12), (5, 9, 1 << 13, (1 << 15) - 1));
        // 10- and 12-bit share `offset`/`limit` *because* the rounding shift moves with the depth;
        // that coincidence is what makes a mistake in either exponent hard to see elsewhere.
        for bd in [8u32, 10, 12] {
            let (round0, _, offset, limit) = wiener_rounding(bd);
            assert_eq!(offset, 1 << (bd + 7 - round0 - 1), "{bd}-bit offset");
            assert_eq!(limit, (1 << (bd + 1 + 7 - round0)) - 1, "{bd}-bit limit");
        }
    }

    use gamut_color::ChromaSubsampling;

    use super::*;

    /// The 4:4:4 geometry of a `w`x`h` frame whose coded grid is `cw`x`ch`, as the tests below use
    /// it: every plane full resolution, so all three share one `PlaneGeom`.
    fn geom444(w: usize, h: usize, mi_cols: usize, mi_rows: usize) -> [PlaneGeom; 3] {
        PlaneGeom::frame(w, h, mi_cols, mi_rows, ChromaSubsampling::Cs444)
    }

    #[test]
    fn superres_x0_matches_spec() {
        // §7.16.3 initial subpel offset, keyed by (FrameWidth, UpscaledWidth) with the matching step.
        // The encoder's superres geometry feeds the oracle upscale, but the `& 0x3fff` wrap can mask a
        // mis-signed `err` or a flipped shift, so the offset is pinned directly here.
        for &(in_w, out_w, want) in &[
            (8usize, 10usize, 14876i32),
            (12, 16, 14465),
            (40, 48, 15156),
        ] {
            let step = (((in_w << 14) + out_w / 2) / out_w) as i32;
            assert_eq!(superres_x0(in_w, out_w, step), want, "x0({in_w}->{out_w})");
        }
    }

    #[test]
    fn superres_downscale_box_averages() {
        // Box-average downscale — the encoder's source preparation, which (unlike the upscale) is the
        // encoder's free choice and so is not bit-exact-bound by dav1d. Pins the box bounds
        // (`x0 = x*src/dst`, `x1 = (x+1)*src/dst`) and the averaging division, including uneven boxes.
        assert_eq!(
            superres_downscale_plane(&[10, 20, 30, 40, 50], 5, 2, 1),
            [15, 40]
        );
        assert_eq!(
            superres_downscale_plane(&[10, 20, 30, 40, 50, 60, 70, 80], 4, 2, 2),
            [15, 35, 55, 75]
        );
        assert_eq!(
            superres_downscale_plane(&[12, 24, 36, 48], 4, 3, 1),
            [12, 24, 42]
        );
    }

    #[test]
    fn cdef_direction_finds_dominant_direction() {
        // §7.15.2 direction search on an 8×8 luma ramp: a horizontal gradient, a vertical one, and a
        // 45° diagonal each pick a distinct direction with a pinned variance. Covers the `- 128`
        // centring and the partial/cost index math (the dav1d oracle does not isolate the search).
        let ramp = |f: fn(usize, usize) -> i32| -> Vec<u16> {
            (0..64)
                .map(|p| (100 + f(p % 8, p / 8) * 4) as u16)
                .collect()
        };
        assert_eq!(
            cdef_direction(&ramp(|x, _| x as i32), 8, 0, 0, 0),
            (6, 4410)
        );
        assert_eq!(
            cdef_direction(&ramp(|_, y| y as i32), 8, 0, 0, 0),
            (2, 4410)
        );
        assert_eq!(
            cdef_direction(&ramp(|x, y| (x + y) as i32), 8, 0, 0, 0),
            (0, 8820)
        );
    }

    #[test]
    fn block_all_skip_reads_the_four_cells() {
        // True only when all four 4×4 cells of the 8×8 block are skip; clearing any single cell makes
        // it false, pinning each of the four MI indices.
        assert!(block_all_skip(&[1, 1, 1, 1], 2, 0, 0));
        assert!(!block_all_skip(&[0, 1, 1, 1], 2, 0, 0)); // (r4, c4)
        assert!(!block_all_skip(&[1, 0, 1, 1], 2, 0, 0)); // (r4, c4 + 1)
        assert!(!block_all_skip(&[1, 1, 0, 1], 2, 0, 0)); // (r4 + 1, c4)
        assert!(!block_all_skip(&[1, 1, 1, 0], 2, 0, 0)); // (r4 + 1, c4 + 1)
        // A non-zero origin in a 3×3 grid exercises the row stride `r4 * mi_cols`.
        let mut grid = vec![1u8; 9];
        assert!(block_all_skip(&grid, 3, 1, 1)); // cells (1,1),(1,2),(2,1),(2,2)
        grid[2 * 3 + 2] = 0;
        assert!(!block_all_skip(&grid, 3, 1, 1));
    }

    /// The levels [`choose_deblock_level`] measures for a distortion `sse`, in the order it
    /// measures them, and the level it chooses.
    fn deblock_search(sse: impl Fn(u8) -> u64) -> (Vec<u8>, u8) {
        let mut tried = Vec::new();
        let chosen = choose_deblock_level(|level| {
            tried.push(level);
            sse(level)
        });
        (tried, chosen)
    }

    #[test]
    fn the_deblock_search_sweeps_then_halves_its_step_around_the_best() {
        // A bowl with its floor at 27: the sweep finds 24 and the step-4 refinement 28 (beating
        // 20). Each level is measured once — a refinement's revisit of a measured level is not
        // measured again.
        let (tried, chosen) = deblock_search(|l| u64::from(l.abs_diff(27)) * 10 + 5);
        // Step 2 measures 26, which ties 28 and wins as the lower level, so step 1 refines
        // around 26.
        assert_eq!(chosen, 27);
        assert_eq!(
            tried,
            [0, 8, 16, 24, 32, 40, 48, 56, 63, 20, 28, 26, 30, 25, 27]
        );
    }

    #[test]
    fn the_deblock_search_reaches_both_ends_and_keeps_the_lowest_of_a_tie() {
        // Filtering only ever hurts: off wins, and the refinement around 0 measures nothing new
        // below it.
        assert_eq!(deblock_search(u64::from).1, 0);
        // Ever-stronger filtering helps: the refinement around 63 is capped at 63 rather than
        // measuring past the largest level.
        let (tried, chosen) = deblock_search(|l| u64::from(63 - l));
        assert_eq!(chosen, 63);
        assert!(tried.iter().all(|&l| l <= MAX_LOOP_FILTER));
        assert_eq!(
            tried[9..],
            [59, 61, 62],
            "below 63 by 4, 2 and 1, nothing above"
        );
        // A flat distortion: every level ties, and the lowest — off — is kept.
        assert_eq!(deblock_search(|_| 7).1, 0);
        // A tie between 16 and 24 keeps 16, the earlier and lower.
        let (_, chosen) = deblock_search(|l| if l == 16 || l == 24 { 1 } else { 9 });
        assert_eq!(chosen, 16);
    }

    #[test]
    fn the_cdef_search_chooses_the_luma_and_chroma_pairs_on_their_own_errors() {
        // Luma is best at (6, 2) and chroma at (11, 4); every one of the 64 pairs is measured,
        // once, primary-major.
        let mut tried = Vec::new();
        let chosen = choose_cdef(|pri, sec| {
            tried.push((pri, sec));
            let d = |p: u8, s: u8| u64::from(pri.abs_diff(p)) * 8 + u64::from(sec.abs_diff(s));
            (d(6, 2), d(11, 4))
        });
        assert_eq!(
            chosen,
            CdefStrengths {
                y_pri: 6,
                y_sec: 2,
                uv_pri: 11,
                uv_sec: 4,
            }
        );
        let every: Vec<(u8, u8)> = (0..=15u8)
            .flat_map(|p| CDEF_SEC_STRENGTHS.map(|s| (p, s)))
            .collect();
        assert_eq!(tried, every);
        // A tie keeps the weakest pair: off, when nothing helps.
        assert_eq!(choose_cdef(|_, _| (3, 3)), CdefStrengths::default());
        // The secondary strength alone can win, at the largest primary.
        let chosen = choose_cdef(|pri, sec| {
            let y = u64::from(pri != 15 || sec != 4);
            (y, u64::from(sec != 1))
        });
        assert_eq!((chosen.y_pri, chosen.y_sec), (15, 4));
        assert_eq!((chosen.uv_pri, chosen.uv_sec), (0, 1));
    }

    #[test]
    fn a_restoration_unit_is_the_one_of_the_samples_four_by_four_block() {
        // §7.17.1: `unitRow = Min(unitRows - 1, (row * MI_SIZE + 8) / unitSize)` — rows are
        // offset up by 8 and taken per 4×4 block — and `unitCol = Min(unitCols - 1, x / unitSize)`.
        let lr = LrUnits {
            cols: 3,
            rows: 2,
            wiener: vec![false; 6],
        };
        assert_eq!(lr.unit_of(0, 0), 0);
        assert_eq!(lr.unit_of(255, 0), 0);
        assert_eq!(lr.unit_of(256, 0), 1);
        assert_eq!(lr.unit_of(511, 0), 1);
        assert_eq!(lr.unit_of(512, 0), 2);
        // The last column absorbs everything to its right.
        assert_eq!(lr.unit_of(900, 0), 2);
        // Row 247 is in the 4×4 block starting at 244: (244 + 8) / 256 = 0. Row 248 starts the
        // block whose offset row reaches 256, so it — and 251, in the same block — is in row 1.
        assert_eq!(lr.unit_of(0, 247), 0);
        assert_eq!(lr.unit_of(0, 248), 3);
        assert_eq!(lr.unit_of(0, 251), 3);
        // The last row absorbs everything below it.
        assert_eq!(lr.unit_of(300, 2000), 4);
        assert_eq!(lr.unit_of(600, 600), 5);
    }

    #[test]
    fn each_restoration_unit_is_restored_only_where_restoring_it_removes_error() {
        // A 520×4 luma frame on a 528 stride: two columns of units — the 8 columns past 512 are
        // absorbed into the second — and one row. Unit 0 is closer to the source restored, unit 1
        // closer left alone; the coded-grid padding past column 520 holds a value that would make
        // restoring unit 1 win, were it counted.
        let (w, h, stride) = (520usize, 4usize, 528usize);
        let source = vec![100u8; w * h];
        let mut cdef = vec![0u16; stride * h];
        let mut restored = vec![100u16; stride * h];
        for y in 0..h {
            for x in 0..w {
                let (off, on) = if x < 256 { (104, 101) } else { (101, 104) };
                cdef[y * stride + x] = off;
                restored[y * stride + x] = on;
            }
        }
        let lr = choose_lr_units(&source, &cdef, &restored, stride, w, h);
        assert_eq!(
            lr,
            LrUnits {
                cols: 2,
                rows: 1,
                wiener: vec![true, false],
            }
        );
        assert!(lr.any());

        // Applying it copies the restored samples of unit 0 only, and only the visible ones.
        let mut out = cdef.clone();
        apply_lr_units(&lr, &mut out, &restored, stride, w, h);
        for y in 0..h {
            for x in 0..stride {
                let i = y * stride + x;
                let want = if x < 256 { restored[i] } else { cdef[i] };
                assert_eq!(out[i], want, "({x}, {y})");
            }
        }

        // One unit whose restored and unrestored errors tie is left alone, and a frame with no
        // restored unit has none to signal.
        let tie = choose_lr_units(&[100u16; 64], &[101; 64], &[99; 64], 8, 8, 8);
        assert_eq!(tie.wiener, [false]);
        assert!(!tie.any());
    }

    #[test]
    fn narrow_filter_smooths_a_small_step() {
        // p1,p0,q0,q1 = 100,100,108,108 (a step of 8). At lvl = 10 (limit 10, blimit 34, thresh 0)
        // the filterMask passes and, with no high edge variance, two samples each side are adjusted
        // toward a gradient. Hand-traced: filter = 24, filter1 = filter2 = 3, f = 2.
        let mut buf = [100u16, 100, 108, 108];
        filter_sample(
            &mut buf,
            2,
            1,
            4,
            true,
            Strength {
                limit: 10,
                blimit: 34,
                thresh: 0,
                bit_depth: 8,
            },
        );
        assert_eq!(buf, [102, 103, 105, 106]);
    }

    #[test]
    fn narrow_filter_skips_large_step() {
        // A step of 20 exceeds blimit at lvl 10, so filterMask is 0 and nothing changes.
        let mut buf = [100u16, 100, 120, 120];
        filter_sample(
            &mut buf,
            2,
            1,
            4,
            true,
            Strength {
                limit: 10,
                blimit: 34,
                thresh: 0,
                bit_depth: 8,
            },
        );
        assert_eq!(buf, [100, 100, 120, 120]);
    }

    #[test]
    fn constrain_soft_thresholds() {
        // threshold 4, damping 3 ⇒ dampingAdj = 3 - floor_log2(4) = 1.
        assert_eq!(constrain(2, 4, 3), 2); // clamp(4 - 1, 0, 2)
        assert_eq!(constrain(-3, 4, 3), -3); // -clamp(4 - 1, 0, 3)
        assert_eq!(constrain(10, 4, 3), 0); // clamp(4 - 5, 0, 10) = 0
        assert_eq!(constrain(7, 0, 3), 0); // threshold 0 ⇒ 0
    }

    #[test]
    fn cdef_leaves_flat_unchanged_but_filters_structure() {
        // A flat plane: every neighbour difference is zero, so constrain is zero and CDEF is identity
        // even with non-zero strengths.
        let flat = [
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
        ];
        // 16×16 ⇒ 4×4 MI grid; no block is skip, so CDEF visits every 8×8.
        let mi_skip = vec![0u8; 4 * 4];
        let g = geom444(16, 16, 4, 4);
        let strong = CdefStrengths {
            y_pri: 12,
            y_sec: 2,
            uv_pri: 12,
            uv_sec: 2,
        };
        let run = |planes: &[Vec<u16>; 3]| {
            let blocks = CdefBlocks::analyse(&planes[0], &g, &mi_skip, 4, 8);
            cdef(planes, &g, &blocks, strong, 3, 8)
        };
        assert_eq!(run(&flat), flat);

        // CDEF only attenuates *small* oscillations (large diffs are constrained away to preserve
        // edges). A low-amplitude vertical stripe gives the direction search a clear direction and a
        // small enough neighbour difference for the constrained sum to be non-zero ⇒ samples change.
        let mut planes = [
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
        ];
        for y in 0..16 {
            for x in 0..16 {
                planes[0][y * 16 + x] = if x % 2 == 0 { 126 } else { 132 };
            }
        }
        let out = run(&planes);
        assert_ne!(
            out[0], planes[0],
            "CDEF should dering a low-amplitude oscillation"
        );
    }

    #[test]
    fn cdef_leaves_a_skipped_block_alone() {
        // The same low-amplitude stripe, but the top-left 8×8's four 4×4 cells are all `skip`
        // (§7.15.1): that block keeps every sample while the rest of the plane is filtered — the
        // analysis records it as skipped, and the filter walks the blocks in the analysis's order.
        let mut planes = [
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
        ];
        for y in 0..16 {
            for x in 0..16 {
                planes[0][y * 16 + x] = if x % 2 == 0 { 126 } else { 132 };
            }
        }
        let mut mi_skip = vec![0u8; 4 * 4];
        for (r, c) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            mi_skip[r * 4 + c] = 1;
        }
        let g = geom444(16, 16, 4, 4);
        let blocks = CdefBlocks::analyse(&planes[0], &g, &mi_skip, 4, 8);
        assert_eq!(blocks.0.len(), 4);
        assert!(blocks.0[0].is_none());
        assert!(blocks.0[1..].iter().all(Option::is_some));
        // The stripe runs along the primary direction, so it is the secondary taps, across it,
        // that dering it.
        let strengths = CdefStrengths {
            y_pri: 12,
            y_sec: 2,
            ..CdefStrengths::default()
        };
        let out = cdef(&planes, &g, &blocks, strengths, 3, 8);
        for y in 0..16 {
            for x in 0..16 {
                let i = y * 16 + x;
                if x < 8 && y < 8 {
                    assert_eq!(out[0][i], planes[0][i], "skipped ({x}, {y})");
                }
            }
        }
        assert_ne!(out[0][8 * 16 + 8..], planes[0][8 * 16 + 8..]);
    }

    #[test]
    fn deblock_leaves_flat_image_unchanged_but_filters_edges() {
        // Flat plane: every difference is zero, so the filter is a no-op.
        let mut flat = [
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
            vec![128u16; 16 * 16],
        ];
        // 16×16 ⇒ 4×4 MI grid, all 4×4 transforms (tx_log2 = 2 everywhere ⇒ narrow filter). The
        // 4×4 blocks have `mi_bsl = 0`, so chroma tx is also 4×4 (log2 = 0 + 2).
        let tx_log2 = vec![2u8; 4 * 4];
        let mi_bsl = vec![0u8; 4 * 4];
        let mi_dlf = vec![0i8; 4 * 4];
        let g = geom444(16, 16, 4, 4);
        deblock(
            &mut flat,
            &g,
            ChromaSubsampling::Cs444,
            4,
            &tx_log2,
            &tx_log2,
            &mi_bsl,
            &mi_bsl,
            &mi_dlf,
            16,
            8,
        );
        assert!(flat[0].iter().all(|&v| v == 128));

        // A vertical step at column 8 gets smoothed across the x = 8 boundary.
        let mut planes = [
            vec![0u16; 16 * 16],
            vec![0u16; 16 * 16],
            vec![0u16; 16 * 16],
        ];
        for y in 0..16 {
            for x in 0..16 {
                planes[0][y * 16 + x] = if x < 8 { 100 } else { 108 };
            }
        }
        let before = planes[0].clone();
        deblock(
            &mut planes,
            &g,
            ChromaSubsampling::Cs444,
            4,
            &tx_log2,
            &tx_log2,
            &mi_bsl,
            &mi_bsl,
            &mi_dlf,
            16,
            8,
        );
        assert_ne!(
            planes[0], before,
            "deblock should modify samples near the edge"
        );
        // Samples far from any 4-boundary are untouched; the edge column pair is pulled together.
        assert!(planes[0][8] < 108 && planes[0][7] > 100);

        // At frame level 0 §7.4 does not run the loop filter at all, even where a superblock's
        // DeltaLF would raise its own level above 0.
        let mut off = [before.clone(), vec![0u16; 16 * 16], vec![0u16; 16 * 16]];
        deblock(
            &mut off,
            &g,
            ChromaSubsampling::Cs444,
            4,
            &tx_log2,
            &tx_log2,
            &mi_bsl,
            &mi_bsl,
            &[1i8; 4 * 4],
            0,
            8,
        );
        assert_eq!(off[0], before);
    }
}
