//! Per-plane sample geometry (AV1 §5.5.2 `subsampling_x`/`subsampling_y`, §5.11.34 plane bases).
//!
//! Every subsampling shift in the encoder lives here. That is deliberate: at 4:4:4 both shifts are
//! zero, so an inline `>> ss_x` written at a call site is indistinguishable from the identity and
//! its mutants are unkillable. Behind [`PlaneGeom`] the same arithmetic is unit-testable at
//! `ss = 1`, which is the only way the mutation gate can see it before the subsampled coding path
//! lands (issues #390 / #391).

use gamut_color::ChromaSubsampling;

/// The sample geometry of one coded plane.
///
/// Two dimensions, not one, and they round differently — this is the subtlety the whole module
/// exists to hold:
///
/// - **Visible** (`w`, `h`) is the source/display extent, `ceil(luma / (1 << ss))`. Ceiling,
///   because an odd luma dimension keeps its half-covering edge chroma sample.
/// - **Coded** (`coded_w`, `coded_h`) is the padded MI-grid extent that `recon` is allocated at and
///   strided by. `mi_cols`/`mi_rows` are always even (`2 * ((n + 7) >> 3)`), so the coded extent is
///   a multiple of 8 luma samples and halving it is **exact** — a plain `>>`, with nothing to round.
///
/// Using one rounding rule for both is wrong in both directions, so they are computed separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaneGeom {
    /// Visible plane width in samples.
    pub w: usize,
    /// Visible plane height in samples.
    pub h: usize,
    /// Coded plane width — the row stride of this plane's reconstruction buffer.
    pub coded_w: usize,
    /// Coded plane height.
    pub coded_h: usize,
    /// `subsampling_x` for this plane (always 0 for luma).
    pub ss_x: u32,
    /// `subsampling_y` for this plane (always 0 for luma).
    pub ss_y: u32,
}

impl PlaneGeom {
    /// The three plane geometries of a frame, `[luma, u, v]`.
    ///
    /// `width`/`height` are the luma display dimensions and `mi_cols`/`mi_rows` the luma MI grid.
    /// Luma always has `ss_x = ss_y = 0`; the chroma planes take their shifts from `subsampling`.
    ///
    /// Monochrome ([`ChromaSubsampling::Cs400`]) has **no** chroma planes, so both chroma entries
    /// are zero-extent. `factors()` reports `(2, 2)` there because that is the AV1 §5.5.2
    /// header inference, not a geometry — using it to size a plane would allocate half-sized
    /// chroma buffers for planes that are never coded. Zero extent instead makes every
    /// extent-bounded loop (deblocking, for one) a no-op without a plane-count test of its own.
    pub(crate) fn frame(
        width: usize,
        height: usize,
        mi_cols: usize,
        mi_rows: usize,
        subsampling: ChromaSubsampling,
    ) -> [PlaneGeom; 3] {
        if subsampling == ChromaSubsampling::Cs400 {
            let luma = PlaneGeom {
                w: width,
                h: height,
                coded_w: mi_cols * 4,
                coded_h: mi_rows * 4,
                ss_x: 0,
                ss_y: 0,
            };
            let none = PlaneGeom {
                w: 0,
                h: 0,
                coded_w: 0,
                coded_h: 0,
                ss_x: 1,
                ss_y: 1,
            };
            return [luma, none, none];
        }
        let (fx, fy) = subsampling.factors();
        let (sx, sy) = (fx.trailing_zeros(), fy.trailing_zeros());
        let plane = |ss_x: u32, ss_y: u32| PlaneGeom {
            // Ceiling on the visible axes: a 5-sample-wide luma plane has 3 chroma samples.
            w: width.div_ceil(1 << ss_x),
            h: height.div_ceil(1 << ss_y),
            // Exact on the coded axes: `mi_cols * 4` is a multiple of 8.
            coded_w: (mi_cols * 4) >> ss_x,
            coded_h: (mi_rows * 4) >> ss_y,
            ss_x,
            ss_y,
        };
        [plane(0, 0), plane(sx, sy), plane(sx, sy)]
    }

    /// Number of samples in this plane's reconstruction buffer.
    pub(crate) fn len(self) -> usize {
        self.coded_w * self.coded_h
    }

    /// The luma MI column covering this plane's sample column `x` — `(x << ss_x) >> 2`.
    ///
    /// The MI grid is defined on luma, so a chroma coordinate is scaled up before it is divided
    /// into 4-sample cells. At `ss_x = 0` this is the plain `x >> 2`.
    pub(crate) fn mi_col(self, x: usize) -> usize {
        (x << self.ss_x) >> 2
    }

    /// The luma MI row covering this plane's sample row `y` — `(y << ss_y) >> 2`.
    pub(crate) fn mi_row(self, y: usize) -> usize {
        (y << self.ss_y) >> 2
    }

    /// §7.14.2: the MI cell whose mode info governs a deblock edge at this plane's sample
    /// `(x, y)` — `(mi_col(x) | ss_x, mi_row(y) | ss_y)`.
    ///
    /// The `| ss` selects the **odd** cell of each subsampled group: a 4:2:0 chroma edge takes the
    /// block size and filter level of the bottom-right MI of its 2x2 luma group, not the top-left.
    /// Identity at 4:4:4, and the difference is invisible unless the group's cells disagree — which
    /// they do across a superblock boundary carrying a per-SB `DeltaLF`.
    pub(crate) fn deblock_mi(self, x: usize, y: usize) -> (usize, usize) {
        (
            self.mi_col(x) | self.ss_x as usize,
            self.mi_row(y) | self.ss_y as usize,
        )
    }

    /// Maps a **luma** sample position into this plane's own coordinates (`x >> ss_x`,
    /// `y >> ss_y`).
    ///
    /// A method rather than an inline shift on purpose: at 4:4:4 both shifts are zero, so
    /// `>>` and `<<` are indistinguishable at every call site the encoder can currently reach.
    /// Here the direction is pinned by a test at `ss = 1`.
    pub(crate) fn scale_pos(self, x: usize, y: usize) -> (usize, usize) {
        (x >> self.ss_x, y >> self.ss_y)
    }

    /// Maps a **luma** block extent into this plane's own extent (`w >> ss_x`, `h >> ss_y`) — the
    /// §7.15.1 chroma CDEF block, for instance, is `(8 >> ss_x) x (8 >> ss_y)`.
    ///
    /// Separate from [`scale_pos`](Self::scale_pos) despite the identical arithmetic: a position
    /// and an extent are different quantities, and under formats gamut does not yet emit they stop
    /// agreeing (a 1-sample extent cannot halve).
    pub(crate) fn scale_extent(self, w: usize, h: usize) -> (usize, usize) {
        (w >> self.ss_x, h >> self.ss_y)
    }
}

/// §5.11.38 `get_plane_residual_size`: the residual block size for `plane` of a `bw` x `bh` **luma**
/// block, or `None` for the spec's `BLOCK_INVALID`.
///
/// This is a table (`Subsampled_Size`), **not** `bw >> ss_x, bh >> ss_y`: it clamps to a 4-sample
/// minimum, so an 8x4 luma block in 4:2:0 has a 4x4 chroma residual rather than 4x2. The sub-8x8
/// cases are exactly the ones subsampling makes reachable, so a shift-based derivation is wrong
/// where it matters most.
///
/// `None` carries a conformance requirement (§6.10.4): *"it is a requirement of bitstream
/// conformance that `get_plane_residual_size(subSize, 1)` is not equal to `BLOCK_INVALID` every
/// time subSize is computed"*. Under 4:2:2 every taller-than-wide block is invalid — halving the
/// width of an 8x32 block would imply a 4x32 chroma block, an aspect ratio AV1 does not code — so
/// the partition search must not emit one.
pub(crate) const fn plane_residual_size(
    bw: usize,
    bh: usize,
    plane: usize,
    ss: ChromaSubsampling,
) -> Option<(usize, usize)> {
    if plane == 0 {
        return Some((bw, bh));
    }
    // Columns of `Subsampled_Size[bsize][1][0]` (4:2:2) and `[bsize][1][1]` (4:2:0). The
    // `[0][1]` column (4:4:0) is omitted: AV1 §5.5.2 cannot signal `subsampling_x = 0` with
    // `subsampling_y = 1`, and `gamut_avif::Av1Config` already rejects that pair as inexpressible.
    let (c422, c420) = match (bw, bh) {
        (4, 4) => (Some((4, 4)), Some((4, 4))),
        (4, 8) => (None, Some((4, 4))),
        (8, 4) => (Some((4, 4)), Some((4, 4))),
        (8, 8) => (Some((4, 8)), Some((4, 4))),
        (8, 16) => (None, Some((4, 8))),
        (16, 8) => (Some((8, 8)), Some((8, 4))),
        (16, 16) => (Some((8, 16)), Some((8, 8))),
        (16, 32) => (None, Some((8, 16))),
        (32, 16) => (Some((16, 16)), Some((16, 8))),
        (32, 32) => (Some((16, 32)), Some((16, 16))),
        (32, 64) => (None, Some((16, 32))),
        (64, 32) => (Some((32, 32)), Some((32, 16))),
        (64, 64) => (Some((32, 64)), Some((32, 32))),
        (64, 128) => (None, Some((32, 64))),
        (128, 64) => (Some((64, 64)), Some((64, 32))),
        (128, 128) => (Some((64, 128)), Some((64, 64))),
        (4, 16) => (None, Some((4, 8))),
        (16, 4) => (Some((8, 4)), Some((8, 4))),
        (8, 32) => (None, Some((4, 16))),
        (32, 8) => (Some((16, 8)), Some((16, 4))),
        (16, 64) => (None, Some((8, 32))),
        (64, 16) => (Some((32, 16)), Some((32, 8))),
        _ => (None, None),
    };
    match ss {
        ChromaSubsampling::Cs444 => Some((bw, bh)),
        ChromaSubsampling::Cs422 => c422,
        ChromaSubsampling::Cs420 => c420,
        // Monochrome has no chroma plane to size, and `ChromaSubsampling` is `#[non_exhaustive]`,
        // so a layout added later has no table row yet. One arm, because a separate `Cs400 => None`
        // would be indistinguishable from falling through to this one.
        _ => None,
    }
}

/// §5.11.5 `HasChroma`: whether the block at MI `(mi_row, mi_col)` codes chroma at all.
///
/// Under 4:2:0 a 4x4 luma block covers only a 2x2 chroma area, which is below the 4x4 minimum, so
/// chroma is coded once for the 2x2 group of luma blocks — by the block at the **odd** MI row and
/// column, i.e. the last of the group in decode order. Every other block of the group codes no
/// `uv_mode`, no `cfl_alpha` and no chroma residual. Under 4:2:2 only the column parity applies.
///
/// The height test is evaluated before the width test, exactly as the spec writes it.
pub(crate) fn has_chroma(
    mi_row: usize,
    mi_col: usize,
    bw: usize,
    bh: usize,
    ss: ChromaSubsampling,
) -> bool {
    if matches!(ss, ChromaSubsampling::Cs400) {
        return false; // NumPlanes == 1
    }
    let (fx, fy) = ss.factors();
    let (ss_x, ss_y) = (fx.trailing_zeros(), fy.trailing_zeros());
    // The spec writes this as two sequential tests that both yield 0; since neither branch has a
    // side effect, the disjunction below is equivalent and the evaluation order is immaterial. A
    // `bw`/`bh` of 4 is the spec's `bw4 == 1` / `bh4 == 1`: only a single-MI-cell extent can be
    // shared with a neighbour.
    let shares_a_neighbours_chroma = (bh == 4 && ss_y == 1 && mi_row.is_multiple_of(2))
        || (bw == 4 && ss_x == 1 && mi_col.is_multiple_of(2));
    !shares_a_neighbours_chroma
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mi_cols`/`mi_rows` as `FrameEncoder::new` derives them, so the table below is stated in the
    /// same terms the encoder uses.
    fn mi(width: usize, height: usize) -> (usize, usize) {
        (2 * ((width + 7) >> 3), 2 * ((height + 7) >> 3))
    }

    #[test]
    fn plane_residual_size_is_a_table_not_a_shift() {
        use ChromaSubsampling::{Cs420, Cs422, Cs444};
        // Luma is never subsampled, whatever the format says.
        for ss in [Cs444, Cs422, Cs420] {
            assert_eq!(plane_residual_size(16, 8, 0, ss), Some((16, 8)), "{ss:?}");
        }
        // 4:4:4 chroma is the block itself.
        assert_eq!(plane_residual_size(16, 8, 1, Cs444), Some((16, 8)));

        // The min-4 clamp is why this cannot be `bw >> ss_x, bh >> ss_y`: an 8x4 block in 4:2:0
        // has a 4x4 chroma residual, not 4x2, and a 4x4 block stays 4x4 rather than becoming 2x2.
        assert_eq!(plane_residual_size(8, 4, 1, Cs420), Some((4, 4)));
        assert_eq!(plane_residual_size(4, 4, 1, Cs420), Some((4, 4)));
        assert_eq!(plane_residual_size(4, 8, 1, Cs420), Some((4, 4)));
        // Ordinary halving above that.
        assert_eq!(plane_residual_size(8, 8, 1, Cs420), Some((4, 4)));
        assert_eq!(plane_residual_size(16, 16, 1, Cs420), Some((8, 8)));
        assert_eq!(plane_residual_size(64, 64, 1, Cs420), Some((32, 32)));
        assert_eq!(plane_residual_size(16, 8, 1, Cs420), Some((8, 4)));

        // 4:2:2 halves width only.
        assert_eq!(plane_residual_size(8, 8, 1, Cs422), Some((4, 8)));
        assert_eq!(plane_residual_size(16, 16, 1, Cs422), Some((8, 16)));
        assert_eq!(plane_residual_size(16, 8, 1, Cs422), Some((8, 8)));
        assert_eq!(plane_residual_size(16, 4, 1, Cs422), Some((8, 4)));

        // The §6.10.4 conformance boundary: under 4:2:2 every taller-than-wide block is
        // BLOCK_INVALID, so the partition search must not emit one. Square and wider-than-tall
        // blocks are all valid.
        for (bw, bh) in [
            (4, 8),
            (8, 16),
            (16, 32),
            (32, 64),
            (4, 16),
            (8, 32),
            (16, 64),
        ] {
            assert_eq!(
                plane_residual_size(bw, bh, 1, Cs422),
                None,
                "{bw}x{bh} at 4:2:2"
            );
            assert!(
                plane_residual_size(bw, bh, 1, Cs420).is_some(),
                "{bw}x{bh} is valid at 4:2:0"
            );
        }
        for (bw, bh) in [(4, 4), (8, 8), (16, 16), (8, 4), (16, 8), (32, 16), (16, 4)] {
            assert!(
                plane_residual_size(bw, bh, 1, Cs422).is_some(),
                "{bw}x{bh} is valid at 4:2:2"
            );
        }
        // Monochrome has no chroma plane to size.
        assert_eq!(plane_residual_size(8, 8, 1, ChromaSubsampling::Cs400), None);

        // Every row of `Subsampled_Size`, so no arm can be deleted without a failure. Listed as
        // (luma, 4:2:2, 4:2:0) with `None` for BLOCK_INVALID.
        /// One `Subsampled_Size` row: the luma block, then its 4:2:2 and 4:2:0 chroma residuals.
        type Row = (
            (usize, usize),
            Option<(usize, usize)>,
            Option<(usize, usize)>,
        );
        let table: [Row; 22] = [
            ((4, 4), Some((4, 4)), Some((4, 4))),
            ((4, 8), None, Some((4, 4))),
            ((8, 4), Some((4, 4)), Some((4, 4))),
            ((8, 8), Some((4, 8)), Some((4, 4))),
            ((8, 16), None, Some((4, 8))),
            ((16, 8), Some((8, 8)), Some((8, 4))),
            ((16, 16), Some((8, 16)), Some((8, 8))),
            ((16, 32), None, Some((8, 16))),
            ((32, 16), Some((16, 16)), Some((16, 8))),
            ((32, 32), Some((16, 32)), Some((16, 16))),
            ((32, 64), None, Some((16, 32))),
            ((64, 32), Some((32, 32)), Some((32, 16))),
            ((64, 64), Some((32, 64)), Some((32, 32))),
            ((64, 128), None, Some((32, 64))),
            ((128, 64), Some((64, 64)), Some((64, 32))),
            ((128, 128), Some((64, 128)), Some((64, 64))),
            ((4, 16), None, Some((4, 8))),
            ((16, 4), Some((8, 4)), Some((8, 4))),
            ((8, 32), None, Some((4, 16))),
            ((32, 8), Some((16, 8)), Some((16, 4))),
            ((16, 64), None, Some((8, 32))),
            ((64, 16), Some((32, 16)), Some((32, 8))),
        ];
        for ((bw, bh), want422, want420) in table {
            assert_eq!(
                plane_residual_size(bw, bh, 1, Cs422),
                want422,
                "{bw}x{bh} at 4:2:2"
            );
            assert_eq!(
                plane_residual_size(bw, bh, 1, Cs420),
                want420,
                "{bw}x{bh} at 4:2:0"
            );
            assert_eq!(
                plane_residual_size(bw, bh, 1, Cs444),
                Some((bw, bh)),
                "{bw}x{bh} at 4:4:4"
            );
        }
        // A shape with no row at all (AV1 has no 4x32 block) is not codable.
        assert_eq!(plane_residual_size(4, 32, 1, Cs420), None);
    }

    #[test]
    fn has_chroma_follows_the_mi_parity_of_a_sub_eight_block() {
        use ChromaSubsampling::{Cs420, Cs422, Cs444};
        // 4:4:4 always codes chroma, at every position and size.
        for (r, c) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            assert!(has_chroma(r, c, 4, 4, Cs444), "({r},{c})");
        }
        // 4:2:0: a 4x4 block codes chroma only at odd MI row *and* column — the last of the 2x2
        // group in decode order, which is what makes the covered luma already reconstructed.
        assert!(!has_chroma(0, 0, 4, 4, Cs420));
        assert!(!has_chroma(0, 1, 4, 4, Cs420));
        assert!(!has_chroma(1, 0, 4, 4, Cs420));
        assert!(has_chroma(1, 1, 4, 4, Cs420));
        // The height test is evaluated first, so an 8-wide/4-tall block at an even row is excluded
        // by height even though its width would pass.
        assert!(!has_chroma(0, 0, 8, 4, Cs420));
        assert!(has_chroma(1, 0, 8, 4, Cs420));
        assert!(!has_chroma(0, 0, 4, 8, Cs420));
        assert!(has_chroma(0, 1, 4, 8, Cs420));
        // Any block 8x8 or larger always codes its own chroma.
        for (r, c) in [(0, 0), (1, 1)] {
            assert!(has_chroma(r, c, 8, 8, Cs420), "({r},{c})");
        }
        // 4:2:2 subsamples x only, so only the column parity applies.
        assert!(!has_chroma(0, 0, 4, 4, Cs422));
        assert!(has_chroma(0, 1, 4, 4, Cs422));
        assert!(has_chroma(1, 1, 4, 4, Cs422));
        assert!(has_chroma(0, 0, 8, 4, Cs422));
        // Monochrome has no chroma at all.
        assert!(!has_chroma(1, 1, 16, 16, ChromaSubsampling::Cs400));
    }

    #[test]
    fn luma_is_never_subsampled() {
        for ss in [
            ChromaSubsampling::Cs444,
            ChromaSubsampling::Cs422,
            ChromaSubsampling::Cs420,
        ] {
            let (mc, mr) = mi(17, 13);
            let g = PlaneGeom::frame(17, 13, mc, mr, ss)[0];
            assert_eq!((g.ss_x, g.ss_y), (0, 0), "{ss:?}");
            assert_eq!((g.w, g.h), (17, 13), "{ss:?}");
            assert_eq!((g.coded_w, g.coded_h), (24, 16), "{ss:?}");
        }
    }

    #[test]
    fn monochrome_has_one_plane_and_two_zero_extent_entries() {
        // The whole point of `PlaneGeom` is that the geometry is unit-testable rather than inferred
        // from a call site, and `Cs400` is the one layout whose chroma entries are never read by
        // the coding path — `cdef` skips them via `1..num_planes` and `deblock`'s extent-bounded
        // loops never execute. Without this test every field of both entries is unobservable, so a
        // mutant that changed any of them would survive.
        //
        // Luma is the same geometry it has at 4:4:4: 17x13 ⇒ mi 6x4 ⇒ coded 24x16. The chroma
        // entries are zero-extent, **not** the half-sized planes that `Cs400::factors()`'s
        // header-inferred `(2, 2)` would otherwise produce — that pair is a §5.5.2 signalling
        // convention, not a geometry, and the `ss_x`/`ss_y` below record it without sizing anything.
        let (mc, mr) = mi(17, 13);
        let g = PlaneGeom::frame(17, 13, mc, mr, ChromaSubsampling::Cs400);
        assert_eq!(
            g[0],
            PlaneGeom {
                w: 17,
                h: 13,
                coded_w: 24,
                coded_h: 16,
                ss_x: 0,
                ss_y: 0,
            }
        );
        let none = PlaneGeom {
            w: 0,
            h: 0,
            coded_w: 0,
            coded_h: 0,
            ss_x: 1,
            ss_y: 1,
        };
        assert_eq!(g[1], none);
        assert_eq!(g[2], none);
        // A zero-extent plane allocates nothing, which is what makes the chroma reconstruction
        // buffers empty and every extent-bounded filter loop a no-op.
        assert_eq!(g[1].len(), 0);
        assert_eq!(g[2].len(), 0);
        // Contrast with 4:2:0 at the same size, which *does* size its chroma from `(1, 1)`. Without
        // this the `Cs400` early return would be indistinguishable from falling through.
        let sub = PlaneGeom::frame(17, 13, mc, mr, ChromaSubsampling::Cs420);
        assert_eq!((sub[1].w, sub[1].h), (9, 7));
        assert_eq!((sub[1].coded_w, sub[1].coded_h), (12, 8));
    }

    #[test]
    fn visible_rounds_up_while_coded_halves_exactly() {
        // The load-bearing asymmetry. 17x13 luma ⇒ mi 6x4 ⇒ coded 24x16.
        //   4:2:0 chroma: visible ceil(17/2) x ceil(13/2) = 9x7, coded 24/2 x 16/2 = 12x8.
        //   4:2:2 chroma: visible 9x13, coded 12x16.
        // A mutant that used `>>` for the visible axis would give 8x6; one that used `div_ceil` for
        // the coded axis would agree here but not at an odd MI count, which cannot occur — so the
        // literal coded values are pinned rather than re-derived.
        let (mc, mr) = mi(17, 13);
        let g420 = PlaneGeom::frame(17, 13, mc, mr, ChromaSubsampling::Cs420)[1];
        assert_eq!((g420.w, g420.h), (9, 7));
        assert_eq!((g420.coded_w, g420.coded_h), (12, 8));
        assert_eq!((g420.ss_x, g420.ss_y), (1, 1));

        let g422 = PlaneGeom::frame(17, 13, mc, mr, ChromaSubsampling::Cs422)[1];
        assert_eq!((g422.w, g422.h), (9, 13));
        assert_eq!((g422.coded_w, g422.coded_h), (12, 16));
        assert_eq!((g422.ss_x, g422.ss_y), (1, 0));

        // The smallest frame: one luma sample still has one chroma sample, on an 8x8 coded grid.
        let (mc, mr) = mi(1, 1);
        let tiny = PlaneGeom::frame(1, 1, mc, mr, ChromaSubsampling::Cs420)[1];
        assert_eq!((tiny.w, tiny.h), (1, 1));
        assert_eq!((tiny.coded_w, tiny.coded_h), (4, 4));

        // 4:4:4 leaves both extents alone, and both chroma planes share one geometry.
        let (mc, mr) = mi(64, 64);
        let g = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs444);
        assert_eq!(g[1], g[2]);
        assert_eq!((g[1].w, g[1].coded_w), (64, 64));
    }

    #[test]
    fn coded_extent_stays_a_whole_number_of_mi_cells() {
        // Invariant the exact `>>` relies on: the coded extent is a multiple of 4 for every plane
        // and every layout, and never smaller than the visible extent.
        for w in 1..=17usize {
            for h in 1..=17usize {
                let (mc, mr) = mi(w, h);
                for ss in [
                    ChromaSubsampling::Cs444,
                    ChromaSubsampling::Cs422,
                    ChromaSubsampling::Cs420,
                ] {
                    for g in PlaneGeom::frame(w, h, mc, mr, ss) {
                        assert_eq!(g.coded_w % 4, 0, "{w}x{h} {ss:?}");
                        assert_eq!(g.coded_h % 4, 0, "{w}x{h} {ss:?}");
                        assert!(g.w <= g.coded_w, "{w}x{h} {ss:?}");
                        assert!(g.h <= g.coded_h, "{w}x{h} {ss:?}");
                        assert_eq!(g.len(), g.coded_w * g.coded_h);
                    }
                }
            }
        }
    }

    #[test]
    fn deblock_mi_takes_the_odd_cell_of_a_subsampled_group() {
        let (mc, mr) = mi(64, 64);
        let luma = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs420)[0];
        let c420 = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs420)[1];
        let c422 = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs422)[1];
        // Luma: the plain cell division, unchanged.
        assert_eq!(luma.deblock_mi(0, 0), (0, 0));
        assert_eq!(luma.deblock_mi(4, 8), (1, 2));
        // 4:2:0: chroma sample 4 covers luma 8, MI cell 2 — and the governing cell is the odd one,
        // 3. A missing `| ss` would give 2, which is the same block only when the pair agrees.
        assert_eq!(c420.deblock_mi(4, 4), (3, 3));
        assert_eq!(c420.deblock_mi(0, 0), (1, 1));
        // 4:2:2 subsamples x only, so the row keeps its even cell.
        assert_eq!(c422.deblock_mi(4, 4), (3, 1));
        // The `|` must not be `^`: at a chroma column whose MI cell is already odd, OR keeps it and
        // XOR would step *back* to the even one. Chroma x = 6 maps to luma 12, MI cell 3.
        assert_eq!(c420.deblock_mi(6, 6), (3, 3));
        assert_eq!(c420.deblock_mi(2, 2), (1, 1));
    }

    #[test]
    fn luma_positions_and_extents_scale_down_into_a_plane() {
        let (mc, mr) = mi(64, 64);
        let g = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs420);
        // Luma is the identity in both directions.
        assert_eq!(g[0].scale_pos(16, 24), (16, 24));
        assert_eq!(g[0].scale_extent(8, 8), (8, 8));
        // 4:2:0 halves both axes. Asserting the halved values (not just "not equal") is what
        // distinguishes `>>` from `<<`, which are the same operation at 4:4:4.
        assert_eq!(g[1].scale_pos(16, 24), (8, 12));
        assert_eq!(g[1].scale_extent(8, 8), (4, 4));
        // 4:2:2 halves x only, so a transposed shift is visible here and nowhere else.
        let g422 = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs422);
        assert_eq!(g422[1].scale_pos(16, 24), (8, 24));
        assert_eq!(g422[1].scale_extent(8, 8), (4, 8));
    }

    #[test]
    fn mi_lookup_scales_a_chroma_coordinate_back_to_the_luma_grid() {
        let (mc, mr) = mi(64, 64);
        let luma = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs420)[0];
        let chroma = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs420)[1];
        // Luma: the plain 4-sample cell division.
        assert_eq!(luma.mi_col(0), 0);
        assert_eq!(luma.mi_col(3), 0);
        assert_eq!(luma.mi_col(4), 1);
        assert_eq!(luma.mi_row(9), 2);
        // Chroma at ss = 1: sample 3 sits at luma 6, i.e. MI cell 1 — the case that distinguishes
        // `(x << ss) >> 2` from a bare `x >> 2`, and the reason this lives behind a function.
        assert_eq!(chroma.mi_col(0), 0);
        assert_eq!(chroma.mi_col(1), 0);
        assert_eq!(chroma.mi_col(2), 1);
        assert_eq!(chroma.mi_col(3), 1);
        assert_eq!(chroma.mi_row(2), 1);
        // 4:2:2 subsamples x only, so its rows track luma while its columns do not.
        let c422 = PlaneGeom::frame(64, 64, mc, mr, ChromaSubsampling::Cs422)[1];
        assert_eq!(c422.mi_col(2), 1);
        assert_eq!(c422.mi_row(2), 0);
    }
}
