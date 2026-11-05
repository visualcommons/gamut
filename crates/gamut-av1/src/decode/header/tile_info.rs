//! `tile_info()` (§5.9.15): the tile grid, uniform or explicitly spaced, and the `tile_log2()`
//! (§5.9.16) its limits are derived with.

use gamut_bitstream::BitReader;
use gamut_core::{Error, Result};

use crate::decode::ORIGIN;
use crate::decode::obu::SequenceHeader;

/// `MAX_TILE_WIDTH` (§3), in luma samples.
const MAX_TILE_WIDTH: u32 = 4096;
/// `MAX_TILE_AREA` (§3), in luma samples.
const MAX_TILE_AREA: u32 = 4096 * 2304;
/// `MAX_TILE_ROWS` / `MAX_TILE_COLS` (§3).
const MAX_TILE_ROWS: u32 = 64;
/// See [`MAX_TILE_ROWS`].
const MAX_TILE_COLS: u32 = 64;

/// The tile grid of `tile_info()` (§5.9.15).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TileInfo {
    /// `TileCols`.
    pub tile_cols: usize,
    /// `TileRows`.
    pub tile_rows: usize,
    /// `TileColsLog2`.
    pub tile_cols_log2: u32,
    /// `TileRowsLog2`.
    pub tile_rows_log2: u32,
    /// `MiColStarts[0..=TileCols]`.
    pub mi_col_starts: Vec<u32>,
    /// `MiRowStarts[0..=TileRows]`.
    pub mi_row_starts: Vec<u32>,
    /// `context_update_tile_id`.
    pub context_update_tile_id: u32,
    /// `TileSizeBytes`.
    pub tile_size_bytes: usize,
}

impl TileInfo {
    /// `tile_info()` (§5.9.15), both the uniform and the explicit spacing branch.
    pub(super) fn parse(
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        mi_cols: u32,
        mi_rows: u32,
    ) -> Result<TileInfo> {
        let (sb_shift, sb_cols, sb_rows) = if seq.use_128x128_superblock {
            (5u32, (mi_cols + 31) >> 5, (mi_rows + 31) >> 5)
        } else {
            (4u32, (mi_cols + 15) >> 4, (mi_rows + 15) >> 4)
        };
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

        let mut mi_col_starts = Vec::new();
        let mut mi_row_starts = Vec::new();
        let tile_cols_log2;
        let tile_rows_log2;

        if r.flag()? {
            // uniform_tile_spacing_flag == 1.
            let mut cols_log2 = min_log2_tile_cols;
            while cols_log2 < max_log2_tile_cols {
                if r.flag()? {
                    cols_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
            let mut start_sb = 0;
            while start_sb < sb_cols {
                mi_col_starts.push(start_sb << sb_shift);
                start_sb += tile_width_sb;
            }
            mi_col_starts.push(mi_cols);

            let min_log2_tile_rows = min_log2_tiles.saturating_sub(cols_log2);
            let mut rows_log2 = min_log2_tile_rows;
            while rows_log2 < max_log2_tile_rows {
                if r.flag()? {
                    rows_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
            let mut start_sb = 0;
            while start_sb < sb_rows {
                mi_row_starts.push(start_sb << sb_shift);
                start_sb += tile_height_sb;
            }
            mi_row_starts.push(mi_rows);

            tile_cols_log2 = cols_log2;
            tile_rows_log2 = rows_log2;
        } else {
            // Explicit tile sizes.
            let mut widest_tile_sb = 0;
            let mut start_sb = 0;
            while start_sb < sb_cols {
                mi_col_starts.push(start_sb << sb_shift);
                let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
                let size_sb = r.ns(max_width)? + 1;
                widest_tile_sb = widest_tile_sb.max(size_sb);
                start_sb += size_sb;
            }
            mi_col_starts.push(mi_cols);
            let cols = mi_col_starts.len() - 1;
            tile_cols_log2 = tile_log2(1, cols as u32);

            max_tile_area_sb = if min_log2_tiles > 0 {
                (sb_rows * sb_cols) >> (min_log2_tiles + 1)
            } else {
                sb_rows * sb_cols
            };
            let max_tile_height_sb = (max_tile_area_sb / widest_tile_sb.max(1)).max(1);
            let mut start_sb = 0;
            while start_sb < sb_rows {
                mi_row_starts.push(start_sb << sb_shift);
                let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
                let size_sb = r.ns(max_height)? + 1;
                start_sb += size_sb;
            }
            mi_row_starts.push(mi_rows);
            tile_rows_log2 = tile_log2(1, (mi_row_starts.len() - 1) as u32);
        }

        let tile_cols = mi_col_starts.len() - 1;
        let tile_rows = mi_row_starts.len() - 1;
        // Both are at least 1 by construction: `mi_cols`/`mi_rows` are at least 2 for a frame of
        // at least one sample, so `sb_cols`/`sb_rows` are at least 1 and each start loop above
        // pushes at least one start before its end.

        let (context_update_tile_id, tile_size_bytes) = if tile_cols_log2 > 0 || tile_rows_log2 > 0
        {
            let id = r.f(tile_rows_log2 + tile_cols_log2)?;
            let bytes = r.f(2)? as usize + 1;
            (id, bytes)
        } else {
            (0, 1)
        };
        if context_update_tile_id as usize >= tile_cols * tile_rows {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 tile_info: context_update_tile_id is out of range",
            ));
        }

        Ok(TileInfo {
            tile_cols,
            tile_rows,
            tile_cols_log2,
            tile_rows_log2,
            mi_col_starts,
            mi_row_starts,
            context_update_tile_id,
            tile_size_bytes,
        })
    }
}

/// `tile_log2( blkSize, target )` (§5.9.16): the smallest `k` with `blkSize << k >= target`.
///
/// `blkSize << k >= target` is `1 << k >= ceil(target / blkSize)`, so `k` is the exponent of the
/// next power of two at or above that quotient — computed directly, as the encoder's twin in
/// `crate::headers` is, rather than by the spec's counting loop: its `k += 1` → `k *= 1` mutant
/// never advances `k` from 0 and spun until the gate's timeout (#914). Every caller passes a
/// `blkSize` of at least 1 (`1`, `maxTileWidthSb`, or `maxTileAreaSb`, both §3 constants shifted
/// by at most `2 * 7`).
pub(crate) fn tile_log2(blk_size: u32, target: u32) -> u32 {
    target
        .div_ceil(blk_size)
        .next_power_of_two()
        .trailing_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::header::FrameHeader;
    use crate::decode::header::tests::parse_built;

    #[test]
    fn tile_info_uses_the_128x128_superblock_geometry() {
        // §5.9.15 derives sbCols/sbRows and sbSize from `use_128x128_superblock`; a 128-wide
        // frame is two 64-superblocks but only one 128-superblock, so the two grids differ.
        let base = crate::decode::testutil::StillBuilder {
            width: 128,
            height: 128,
            width_bits: 8,
            height_bits: 8,
            tile_cols_log2: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq64, small) = parse_built(&base);
        assert!(!seq64.use_128x128_superblock);
        assert_eq!(small.tile_info.tile_cols, 2, "two 64x64 superblock columns");
        assert_eq!(small.tile_info.mi_col_starts, vec![0, 16, small.mi_cols]);

        let (seq128, large) = parse_built(&crate::decode::testutil::StillBuilder {
            use_128x128_superblock: true,
            tile_cols_log2: 0,
            ..base
        });
        assert!(seq128.use_128x128_superblock);
        assert_eq!(
            large.tile_info.tile_cols, 1,
            "the same frame is a single 128x128 superblock column"
        );
        assert_eq!(large.tile_info.mi_col_starts, vec![0, large.mi_cols]);
    }

    #[test]
    fn tile_info_derives_the_tile_limits_from_the_superblock_size() {
        // `maxTileWidthSb = MAX_TILE_WIDTH >> sbSize` and
        // `maxTileAreaSb = MAX_TILE_AREA >> (2 * sbSize)` set `minLog2TileCols` and
        // `minLog2Tiles`, which force a minimum number of tiles once a frame is wide or large
        // enough. A frame wider than MAX_TILE_WIDTH cannot be one tile column.
        let wide = crate::decode::testutil::StillBuilder {
            width: 8192,
            height: 128,
            width_bits: 13,
            height_bits: 8,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&wide);
        assert!(
            fh.tile_info.tile_cols >= 2,
            "8192 px exceeds MAX_TILE_WIDTH, so one tile column is illegal"
        );
        assert_eq!(fh.tile_info.tile_cols_log2, 1);

        // A frame whose area exceeds MAX_TILE_AREA forces extra tiles through minLog2Tiles even
        // when each column is narrow enough.
        let big = crate::decode::testutil::StillBuilder {
            width: 4096,
            height: 4096,
            width_bits: 12,
            height_bits: 12,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&big);
        assert!(
            fh.tile_info.tile_cols * fh.tile_info.tile_rows >= 2,
            "a 4096x4096 frame exceeds MAX_TILE_AREA for a single tile"
        );
    }

    #[test]
    fn tile_info_spaces_multiple_tile_rows_uniformly() {
        // 256x256 is 4x4 64x64-superblocks. `tile_rows_log2 = 1` splits it into two tile rows of
        // `ceil(4 / 2) = 2` superblocks, so the second row starts at superblock 2 — MI unit
        // `2 << sbShift` = 32. Every other case in this suite has a single tile row, where both
        // the ceiling term and the `<< sbShift` are unobservable: the loop pushes one start of 0
        // whatever they compute.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 256,
            width_bits: 8,
            height_bits: 8,
            tile_rows_log2: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_rows, 2);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.tile_rows_log2, 1);
        // The columns stay a single tile, so the row spacing is the only thing under test.
        assert_eq!(fh.tile_info.tile_cols, 1);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 64]);
    }

    #[test]
    fn tile_info_reads_explicit_tile_spacing() {
        // `uniform_tile_spacing_flag = 0` codes every tile's width and height as an `ns()` size
        // instead of deriving them from a log2 count (§5.9.15). Neither `gamut-av1`'s encoder nor
        // libaom ever writes this branch — both emit uniform spacing — so `StillBuilder` is the
        // only way to reach it. A 2x2 grid over 4x4 superblocks puts each boundary at superblock
        // 2, MI unit 32.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 256,
            width_bits: 8,
            height_bits: 8,
            explicit_tiles: Some((&[2, 2], &[2, 2])),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_cols, 2);
        assert_eq!(fh.tile_info.tile_rows, 2);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.tile_cols_log2, 1);
        assert_eq!(fh.tile_info.tile_rows_log2, 1);
    }

    #[test]
    fn tile_log2_matches_the_spec_definition() {
        assert_eq!(tile_log2(1, 1), 0);
        assert_eq!(tile_log2(1, 2), 1);
        assert_eq!(tile_log2(1, 3), 2);
        assert_eq!(tile_log2(1, 4), 2);
        assert_eq!(tile_log2(1, 5), 3);
        assert_eq!(tile_log2(64, 64), 0);
        assert_eq!(tile_log2(64, 65), 1);
        assert_eq!(tile_log2(64, 256), 2);
        // `maxTileAreaSb` for 64x64 superblocks against a frame area just either side of a
        // doubling.
        assert_eq!(tile_log2(2304, 4608), 1);
        assert_eq!(tile_log2(2304, 4609), 2);
    }

    #[test]
    fn tile_info_recomputes_the_row_area_limit_from_the_coded_columns() {
        // §5.9.15's explicit-spacing branch recomputes `maxTileAreaSb` *after* the columns are
        // read: `(sbRows * sbCols) >> (minLog2Tiles + 1)` when `minLog2Tiles > 0`, the whole frame
        // area otherwise. That sets `maxTileHeightSb`, the `ns()` bound every row size is coded
        // against, so a wrong area reads the wrong number of bits per row.
        //
        // 3136 px is 49 64x64 superblocks each way. 49 * 49 = 2401 exceeds `maxTileAreaSb` (2304),
        // so `minLog2Tiles` is 1 and the recomputed area is 2401 >> 2 = 600. A widest column of 25
        // superblocks then caps a tile row at 600 / 25 = 24, which is why the rows below are
        // 17 + 16 + 16 and not a single 49. Taking the `else` branch would allow 96, and dividing
        // instead of multiplying would collapse the cap to 1 and split the frame into 49 rows.
        let still = crate::decode::testutil::StillBuilder {
            width: 3136,
            height: 3136,
            width_bits: 12,
            height_bits: 12,
            explicit_tiles: Some((&[25, 24], &[17, 16, 16])),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_cols, 2);
        assert_eq!(fh.tile_info.tile_rows, 3);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 400, 784]);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 272, 528, 784]);
        assert_eq!(fh.tile_info.tile_cols_log2, 1);
        assert_eq!(fh.tile_info.tile_rows_log2, 2);
    }

    #[test]
    fn tile_info_bounds_explicit_rows_by_the_frame_area_below_the_tile_limit() {
        // §5.9.15's explicit branch with `minLog2Tiles == 0` takes the whole `sbRows * sbCols`
        // as `maxTileAreaSb`. A 256x256 frame is 4x4 superblocks; one column 4 wide leaves
        // `maxTileHeightSb = 16 / 4 = 4`, so a single 4-superblock row is coded against `ns(4)`.
        // Halving the area, or adding instead of multiplying, gives a cap of 2 and a narrower
        // `ns()`.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 256,
            width_bits: 8,
            height_bits: 8,
            explicit_tiles: Some((&[4], &[4])),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_rows, 1);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 64]);
    }

    #[test]
    fn context_update_tile_id_must_name_a_tile_of_the_grid() {
        // §6.8.14: `context_update_tile_id` must be less than `TileCols * TileRows`. Three
        // columns code it in `TileColsLog2 = 2` bits, so id 3 is codable but names no tile —
        // and is still below `TileCols + TileRows = 4`.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            width_bits: 8,
            explicit_tiles: Some((&[1, 1, 2], &[1])),
            context_update_tile_id: 2,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_cols, 3);
        assert_eq!(fh.tile_info.context_update_tile_id, 2);

        let past = crate::decode::testutil::StillBuilder {
            context_update_tile_id: 3,
            ..still
        };
        let seq = SequenceHeader::parse(&past.sequence_header()).unwrap();
        let payload = past.frame_obu();
        assert_eq!(
            FrameHeader::parse(&mut BitReader::new(&payload), &seq)
                .unwrap_err()
                .static_message(),
            Some("AV1 tile_info: context_update_tile_id is out of range")
        );
    }
}
