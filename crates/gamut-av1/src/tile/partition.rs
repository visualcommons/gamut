//! The partition layer (§5.11.4): iterating a superblock's partition tree, coding each
//! `partition` symbol with its above/left context, and the bottom-up rate–distortion search that
//! trials a region whole, in rectangular halves and in quadrants — with the block snapshot
//! ([`BlockState`]) that lets every trial be undone.

use super::FrameEncoder;
use super::recon::PlaneState;
use crate::geom::plane_residual_size;

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

/// A region's partition, reached while it was trialled: see [`FrameEncoder::decided_partitions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecidedPartition {
    /// The region: MI `(r, c)` and its side in samples.
    pub(super) at: (usize, usize, usize),
    /// 0 `PARTITION_NONE`, 1 `PARTITION_HORZ`, 2 `PARTITION_VERT` or 3 `PARTITION_SPLIT`.
    pub(super) bp: usize,
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

impl FrameEncoder<'_> {
    pub(super) fn encode_partition(&mut self, r: usize, c: usize, bw: usize) {
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
            let bp = if self.qindex == 0 {
                0 // PARTITION_NONE
            } else if block_exceeds_frame(r, c, num4x4, self.mi_rows, self.mi_cols) {
                // The block extends past the frame edge (only its top/left half is in-frame, which is
                // why `has_rows && has_cols` still hold). AV1 permits a frame-spanning PARTITION_NONE
                // here, but the lossy reconstruction buffer is the MI-frame size, so a single
                // block-size transform would write out of bounds. Force PARTITION_SPLIT — always a
                // legal choice in this branch — so the recursion descends until every coded transform
                // fits (a 4×4 leaf always fits); the out-of-frame leaves are skipped in `residual`.
                3 // PARTITION_SPLIT
            } else {
                // Lossy and on-screen: PARTITION_NONE, HORZ, VERT or SPLIT, by rate–distortion
                // trial among those the search offers.
                self.choose_partition(r, c, bw, ctx)
            };
            self.sym.encode(bp, self.cdfs.partition(bsl, ctx));
            bp
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
        self.code_partition(r, c, bw, bp);
    }

    /// Codes the `bw × bw` region at MI `(r, c)` as partition `bp` (0 NONE, 1 HORZ, 2 VERT,
    /// 3 SPLIT, `usize::MAX` for a sub-8×8 block, which decodes no partition), once its partition
    /// symbol, if any, is coded: the partition context update, then the blocks, or the four
    /// quadrants recursively.
    fn code_partition(&mut self, r: usize, c: usize, bw: usize, bp: usize) {
        let half = bw / 8;
        let bsl = (bw / 4).trailing_zeros() as usize; // Mi_Width_Log2

        // AbovePartitionContext/LeftPartitionContext update (§5.11.4): a terminal partition (NONE/
        // HORZ/VERT) writes its level's context; PARTITION_SPLIT writes only at 8×8 (its 4×4 children
        // decode no partition), otherwise its children update as they recurse. The forced-NONE sub-8×8
        // case (`bp = MAX`) decodes no partition, so it writes nothing.
        if bw >= 8 && bp != usize::MAX && (bp != 3 || bsl == 1) {
            self.update_partition_ctx(r, c, bsl, bp);
        }

        // §5.11.4 decodes a HORZ (VERT) partition's second half only `if (hasRows)` (`hasCols`),
        // but this encoder offers HORZ and VERT only to a region inside the frame
        // (`encode_partition` forces SPLIT where `block_exceeds_frame`), where both hold; the
        // guards were deleted with their `<` → `<=` twins, which no region could tell apart.
        match bp {
            usize::MAX | 0 => self.encode_block(r, c, bw, bw),
            1 => {
                // PARTITION_HORZ: top then bottom half, each `bw × bw/2`.
                self.encode_block(r, c, bw, bw / 2);
                self.encode_block(r + half, c, bw, bw / 2);
            }
            2 => {
                // PARTITION_VERT: left then right half, each `bw/2 × bw`.
                self.encode_block(r, c, bw / 2, bw);
                self.encode_block(r, c + half, bw / 2, bw);
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

    /// Decides the partition of the fully on-screen lossy `bw × bw` region at MI `(r, c)`
    /// (8×8..64×64), whose partition symbol is coded under context `ctx`: 0 `PARTITION_NONE`,
    /// 1 `PARTITION_HORZ`, 2 `PARTITION_VERT` or 3 `PARTITION_SPLIT`.
    ///
    /// The search is bottom-up by rate–distortion cost: each partition [`Self::partitions_offered`]
    /// names is trialled whole — partition symbol, mode info, every plane's residual — and the
    /// cheapest taken, the earliest offered on a tie (so `PARTITION_NONE` before the others). A
    /// `PARTITION_SPLIT` trial codes each quadrant as this same decision chooses for it, so the
    /// quadrants are decided first, inside the trial. Every decision a trial reaches — a
    /// sub-region's partition here ([`FrameEncoder::decided_partitions`]), a block's modes in
    /// [`Self::encode_block`] ([`FrameEncoder::decided`]) — is kept, and taken as reached when the
    /// region is coded again, by an enclosing trial or for real: each region is trialled once per
    /// alternative rather than once per enclosing level, so the search costs time linear in its
    /// depth, not exponential.
    ///
    /// A decision is reused against the CDFs it was priced from, not the ones the stream has
    /// adapted to by the time the region is coded for real; that is the cost of not deciding it
    /// twice.
    fn choose_partition(&mut self, r: usize, c: usize, bw: usize, ctx: usize) -> usize {
        let at = (r, c, bw);
        let bp = match self.decided_partitions.iter().position(|d| d.at == at) {
            Some(i) => self.decided_partitions.swap_remove(i).bp,
            None => self.trial_partitions(r, c, bw, ctx),
        };
        let decided = DecidedPartition { at, bp };
        if self.sym.tally.is_some() {
            self.decided_partitions.push(decided);
        } else {
            self.sb_decisions.partitions.push(decided);
        }
        bp
    }

    /// The partitions the search offers the fully on-screen lossy `bw × bw` region at MI `(r, c)`,
    /// in the order they win ties: `PARTITION_NONE`, then `PARTITION_HORZ` and `PARTITION_VERT`
    /// where the search codes rectangular halves at this size and the halves are codable
    /// (§6.10.4: under 4:2:2 no taller-than-wide block is), then `PARTITION_SPLIT` from the
    /// search's `split_from` up.
    fn partitions_offered(&self, bw: usize) -> Vec<usize> {
        let mut offered = vec![0];
        if self.search.rect_sizes.contains(&bw) {
            let hp = bw / 2;
            if self.rect_partition_is_codable(bw, hp) {
                offered.push(1);
            }
            if self.rect_partition_is_codable(hp, bw) {
                offered.push(2);
            }
        }
        if bw >= self.search.split_from {
            offered.push(3);
        }
        offered
    }

    /// Trials every partition [`Self::partitions_offered`] names for the region at MI `(r, c)`
    /// and returns the cheapest; a region offered only one is coded that way untrialled.
    fn trial_partitions(&mut self, r: usize, c: usize, bw: usize, ctx: usize) -> usize {
        let offered = self.partitions_offered(bw);
        if let [only] = offered[..] {
            return only;
        }
        let bsl = (bw / 4).trailing_zeros() as usize;
        // `min_by_key` keeps the first of equal costs: a tie goes to the earlier offer.
        offered
            .into_iter()
            .map(|bp| {
                let cost = self.trial_cost(r, c, bw, |e| {
                    e.sym.encode(bp, e.cdfs.partition(bsl, ctx));
                    e.code_partition(r, c, bw, bp);
                });
                (cost, bp)
            })
            .min_by_key(|&(cost, _)| cost)
            .map_or(0, |(_, bp)| bp)
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
            above_partition: self.above_partition[cols.clone()].to_vec(),
            left_partition: self.left_partition[rows.clone()].to_vec(),
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
        self.above_partition[s.c..s.c + s.above_partition.len()]
            .copy_from_slice(&s.above_partition);
        self.left_partition[s.r..s.r + s.left_partition.len()].copy_from_slice(&s.left_partition);
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
    pub(super) fn mi_maps(&self) -> [&Vec<u8>; 8] {
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

    /// Whether a `bw` x `bh` block has a codable chroma residual under this frame's subsampling
    /// (§5.11.38 / §6.10.4). Always true at 4:4:4 and 4:2:0, and in a monochrome frame — which has
    /// no chroma plane to constrain; false for taller-than-wide blocks at 4:2:2.
    pub(super) fn rect_partition_is_codable(&self, bw: usize, bh: usize) -> bool {
        !self.has_chroma() || plane_residual_size(bw, bh, 1, self.subsampling).is_some()
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::{ChromaSubsampling, Planar8};

    use super::*;
    use crate::encoder::Effort;
    use crate::tile::BlockVariation;
    use crate::tile::tests::{Snapshot, at_effort, busy64, dims, flat_chroma_luma, luma16, primed};
    #[test]
    fn partitions_offered_follow_the_search_and_the_layout() {
        // The full search offers the rectangular halves at 16 and 32 only, and SPLIT down to 8;
        // `Fastest` offers no rectangle and stops choosing SPLIT below 16. §6.10.4: at 4:2:2 the
        // taller-than-wide half of PARTITION_VERT has no codable chroma residual, so it is never
        // offered there, while 4:4:4 and 4:2:0 code it.
        let offered = |ss, effort, bw| {
            let p = flat_chroma_luma(16, ss, |_, _| 128);
            at_effort(&p, 40, effort).partitions_offered(bw)
        };
        for ss in [ChromaSubsampling::Cs444, ChromaSubsampling::Cs420] {
            assert_eq!(offered(ss, Effort::Thorough, 8), [0, 3], "{ss:?} 8x8");
            assert_eq!(
                offered(ss, Effort::Thorough, 16),
                [0, 1, 2, 3],
                "{ss:?} 16x16"
            );
            assert_eq!(offered(ss, Effort::Fast, 32), [0, 1, 2, 3], "{ss:?} 32x32");
            assert_eq!(offered(ss, Effort::Thorough, 64), [0, 3], "{ss:?} 64x64");
            assert_eq!(offered(ss, Effort::Fastest, 8), [0], "{ss:?} Fastest 8x8");
            assert_eq!(
                offered(ss, Effort::Fastest, 16),
                [0, 3],
                "{ss:?} Fastest 16x16"
            );
        }
        let cs422 = ChromaSubsampling::Cs422;
        assert_eq!(offered(cs422, Effort::Thorough, 16), [0, 1, 3]);
        assert_eq!(offered(cs422, Effort::Thorough, 32), [0, 1, 3]);
    }

    #[test]
    fn encode_partition_stops_splitting_at_the_preset_floor() {
        // `Mi_Width_Log2` 0 is a 4-sample-wide block. A field of flat 4x4 cells, each its own
        // value, is predicted best one cell at a time, so under the full search it splits down to
        // 4x4 leaves; the `Fastest` preset stops choosing a split below 8x8, and a 64x64 frame
        // forces none.
        let mut rgb = vec![0u8; 64 * 64 * 3];
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let cell = (i % 64 / 4) * 7 + (i / 64 / 4) * 13;
            let v = ((cell * 97) % 251) as u8;
            px.copy_from_slice(&[v, v.wrapping_mul(3), 128]);
        }
        let p = Planar8::from_rgb8_identity(&rgb, dims(64, 64)).unwrap();
        let leaves = |effort| {
            let mut e = at_effort(&p, 40, effort);
            e.clear_block_decoded(0, 0);
            e.encode_partition(0, 0, 64);
            e.mi_bsl.iter().filter(|&&bsl| bsl == 0).count()
        };
        assert!(leaves(Effort::Thorough) > 0, "the full search reaches 4x4");
        assert_eq!(leaves(Effort::Fastest), 0, "no 4x4 leaf by choice");
    }

    #[test]
    fn a_partition_is_the_cheapest_offered_alternative_trialled_whole() {
        // `choose_partition` must answer exactly the first of least cost among the partitions
        // `partitions_offered` names, each priced whole — partition symbol, the blocks' mode
        // info, every plane's residual, and for SPLIT the quadrants as they decide themselves —
        // and leave the encoder untouched. A gentle ramp and a hard edge stay whole, and
        // high-contrast texture splits.
        let ramp = luma16(|x, y| (100 + x + y) as u8);
        let edge = flat_chroma_luma(32, ChromaSubsampling::Cs444, |x, _| {
            if x % 16 < 8 { 30 } else { 210 }
        });
        let busy = busy64();
        let mut seen = [false; 4];
        let cases = [
            (&ramp, 0, 0, 8),
            (&ramp, 0, 0, 16),
            (&edge, 0, 0, 16),
            (&edge, 0, 0, 32),
            (&busy, 2, 4, 8),
            (&busy, 6, 2, 8),
            (&busy, 4, 8, 16),
            (&busy, 8, 0, 32),
        ];
        for (planes, r, c, bw) in cases {
            let mut e = primed(planes, 40);
            let bsl = (bw / 4usize).trailing_zeros() as usize;
            let ctx = e.partition_ctx(r, c, bsl);
            // Each alternative trialled on its own copy, so no decision one trial reaches is
            // reused by the next.
            let costs: Vec<(u64, usize)> = e
                .partitions_offered(bw)
                .into_iter()
                .map(|bp| {
                    let mut alone = e.clone();
                    let cost = alone.trial_cost(r, c, bw, |e| {
                        e.sym.encode(bp, e.cdfs.partition(bsl, ctx));
                        e.code_partition(r, c, bw, bp);
                    });
                    (cost, bp)
                })
                .collect();
            let least = costs.iter().map(|&(cost, _)| cost).min().unwrap();
            let first = costs.iter().find(|&&(cost, _)| cost == least).unwrap().1;
            let before = Snapshot::of(&e);
            let chosen = e.choose_partition(r, c, bw, ctx);
            assert_eq!(chosen, first, "({r}, {c}) {bw}x{bw}: {costs:?}");
            assert!(
                Snapshot::of(&e) == before,
                "choose_partition changed the encoder"
            );
            seen[chosen] = true;
        }
        // A rectangular half codes only the SMOOTH family and PAETH, never palette or a split
        // transform, so on content this small a square alternative always beats it: none of
        // these cases picks one, though each prices both.
        assert_eq!(
            seen,
            [true, false, false, true],
            "the cases reach NONE and SPLIT"
        );
        // A region offered only PARTITION_NONE is coded that way untrialled.
        let mut fastest = at_effort(&busy, 40, Effort::Fastest);
        assert_eq!(fastest.choose_partition(0, 0, 8, 0), 0);
    }

    #[test]
    fn a_decided_partition_is_reused_and_kept_only_while_trialling() {
        // A region the search has already decided is coded as decided, whatever a fresh trial
        // would say; an enclosing trial keeps the decision for the pass that codes it next, and
        // that pass — the real one — consumes it.
        let busy = busy64();
        let mut e = primed(&busy, 40);
        // PARTITION_HORZ, which a fresh 8x8 search is never offered.
        e.decided_partitions.push(DecidedPartition {
            at: (2, 4, 8),
            bp: 1,
        });
        e.sym.tally = Some(0);
        assert_eq!(e.choose_partition(2, 4, 8, 0), 1, "taken while trialling");
        assert_eq!(e.decided_partitions.len(), 1, "kept while trialling");
        e.sym.tally = None;
        assert_eq!(e.choose_partition(2, 4, 8, 0), 1, "taken for real");
        assert!(e.decided_partitions.is_empty(), "consumed for real");
        // A trialled decision is recorded, a real one is not.
        e.sym.tally = Some(0);
        let trialled = e.choose_partition(2, 4, 8, 0);
        assert_eq!(
            e.decided_partitions.last().map(|d| (d.at, d.bp)),
            Some(((2, 4, 8), trialled))
        );
        e.sym.tally = None;
        e.decided_partitions.clear();
        e.choose_partition(6, 2, 8, 0);
        assert!(e.decided_partitions.is_empty(), "nothing recorded for real");
    }

    #[test]
    fn a_trial_puts_back_everything_coding_a_superblocks_first_block_changes() {
        // The first block of a superblock also signals `delta_q`/`delta_lf`, moving the quantizer
        // state; a trial of it must restore that too, along with every per-MI map, context and
        // reconstructed sample it wrote, and must neither code nor adapt anything. The segment id
        // and the deltas are coded only under the block-variation exercise.
        let p = busy64();
        let mut e = at_effort(&p, 40, Effort::Thorough)
            .with_block_variation(BlockVariation::SyntaxExercise);
        e.clear_block_decoded(0, 0);
        e.read_deltas = true;
        let before = Snapshot::of(&e);
        let cost = e.trial_cost(0, 0, 8, |e| e.encode_block(0, 0, 8, 8));
        assert!(cost > 0);
        assert!(Snapshot::of(&e) == before, "trial_cost changed the encoder");
    }

    #[test]
    fn save_block_captures_the_blocks_cells_clipped_to_the_frame() {
        // A trial writes only inside its block, so restoring a wider capture puts back values
        // nothing changed and no trial can tell the two apart; the capture's extent is what
        // `save_block` owes, and it is asserted on what it returns. 64×64 luma: a 16×16 MI grid.
        let p = busy64();
        let e = FrameEncoder::new(&p, 40);
        let extent = |s: &BlockState| {
            (
                s.w4,
                s.above_partition.len(),
                s.left_partition.len(),
                s.maps[0].len(),
            )
        };
        assert_eq!(extent(&e.save_block(0, 0, 8)), (2, 2, 2, 4), "interior 8×8");
        assert_eq!(
            extent(&e.save_block(4, 8, 16)),
            (4, 4, 4, 16),
            "interior 16×16"
        );
        assert_eq!(
            extent(&e.save_block(12, 14, 16)),
            (2, 2, 4, 8),
            "clipped 16×16"
        );
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
}
