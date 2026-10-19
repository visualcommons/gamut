//! The frame OBU and tile group OBU framing (AV1 §5.10, §5.11.1).
//!
//! This is the layer between the frame header and the per-tile symbol decoding: it splits a tile
//! group's payload into the byte range belonging to each tile, following the `TileSizeBytes`
//! little-endian size prefixes that precede every tile but the last.
//!
//! Splitting is kept separate from decoding so the framing can be validated on its own — a
//! truncated or lying tile-size prefix is caught here, before any tile is handed to the symbol
//! decoder, and each returned slice is guaranteed to lie inside the payload.
//!
//! Tiles come back in `TileNum` order, so a consumer indexes the grid with
//! `tileRow = TileNum / TileCols` and `tileCol = TileNum % TileCols` against
//! [`TileInfo::mi_row_starts`]/[`TileInfo::mi_col_starts`] (§5.11.1). Carrying those bounds here
//! would duplicate what [`TileInfo`] already holds.

use gamut_bitstream::BitReader;
use gamut_core::{Error, Result};

use super::ORIGIN;
use super::header::TileInfo;

/// Splits a tile group OBU payload into its tiles (§5.11.1).
///
/// `payload` starts at the tile group header (the `tile_start_and_end_present_flag` bit), which is
/// where [`crate::decode::header::FrameHeader::parse`] leaves an `OBU_FRAME` reader after
/// `byte_alignment()`.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the framing is malformed: a tile-size prefix that runs past
/// the payload, a tile group whose range falls outside the frame's tile grid, or a payload that
/// ends before the declared tiles do.
pub(crate) fn split_tiles<'a>(payload: &'a [u8], info: &TileInfo) -> Result<Vec<&'a [u8]>> {
    let num_tiles = info.tile_cols * info.tile_rows;
    let mut r = BitReader::new(payload);
    let tile_start_and_end_present = num_tiles > 1 && r.flag()?;
    let (tg_start, tg_end) = if num_tiles == 1 || !tile_start_and_end_present {
        (0usize, num_tiles - 1)
    } else {
        let tile_bits = info.tile_cols_log2 + info.tile_rows_log2;
        (r.f(tile_bits)? as usize, r.f(tile_bits)? as usize)
    };
    r.byte_alignment()?;

    if tg_end < tg_start || tg_end >= num_tiles {
        return Err(Error::invalid_input(
            ORIGIN,
            "AV1 tile group: tg_start/tg_end fall outside the tile grid",
        ));
    }
    // A still image is a single frame, so one tile group must carry every tile — a decoder that
    // accepted a partial group would silently emit an incomplete picture.
    if tg_start != 0 || tg_end != num_tiles - 1 {
        return Err(Error::unsupported(
            ORIGIN,
            "AV1 tile group: a still image must carry every tile in one tile group",
        ));
    }

    let mut rest = r.remaining_bytes();
    let mut tiles = Vec::with_capacity(num_tiles);
    for tile_num in tg_start..=tg_end {
        let last = tile_num == tg_end;
        let size = if last {
            rest.len()
        } else {
            if rest.len() < info.tile_size_bytes {
                return Err(Error::invalid_input(
                    ORIGIN,
                    "AV1 tile group: payload ends inside a tile size field",
                ));
            }
            let (prefix, after) = rest.split_at(info.tile_size_bytes);
            rest = after;
            let mut size = 0usize;
            for (i, byte) in prefix.iter().enumerate() {
                size |= (*byte as usize) << (i * 8);
            }
            size + 1
        };
        if size > rest.len() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 tile group: tile size runs past the end of the payload",
            ));
        }
        if size == 0 {
            return Err(Error::invalid_input(ORIGIN, "AV1 tile group: empty tile"));
        }
        let (data, after) = rest.split_at(size);
        rest = after;
        tiles.push(data);
    }
    Ok(tiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-column, one-row grid with 4-byte tile sizes — the shape `gamut-av1`'s encoder emits
    /// for a frame at least two superblocks wide.
    fn two_column_grid() -> TileInfo {
        TileInfo {
            tile_cols: 2,
            tile_rows: 1,
            tile_cols_log2: 1,
            tile_rows_log2: 0,
            mi_col_starts: vec![0, 16, 24],
            mi_row_starts: vec![0, 16],
            context_update_tile_id: 0,
            tile_size_bytes: 4,
        }
    }

    /// A single-tile grid, where no size prefix is coded at all.
    fn single_tile_grid() -> TileInfo {
        TileInfo {
            tile_cols: 1,
            tile_rows: 1,
            tile_cols_log2: 0,
            tile_rows_log2: 0,
            mi_col_starts: vec![0, 16],
            mi_row_starts: vec![0, 16],
            context_update_tile_id: 0,
            tile_size_bytes: 1,
        }
    }

    /// Builds a tile group payload: the `tile_start_and_end_present_flag` byte (when more than one
    /// tile), then each non-final tile prefixed by its `tile_size_minus_1`.
    fn payload(info: &TileInfo, tiles: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        if info.tile_cols * info.tile_rows > 1 {
            out.push(0u8); // tile_start_and_end_present_flag = 0, then byte alignment
        }
        for (i, tile) in tiles.iter().enumerate() {
            if i + 1 < tiles.len() {
                let size_minus_1 = tile.len() - 1;
                for b in 0..info.tile_size_bytes {
                    out.push(((size_minus_1 >> (b * 8)) & 0xff) as u8);
                }
            }
            out.extend_from_slice(tile);
        }
        out
    }

    #[test]
    fn splits_a_two_tile_group_at_the_size_prefix() {
        let info = two_column_grid();
        let first: &[u8] = &[1, 2, 3, 4, 5];
        let second: &[u8] = &[9, 8, 7];
        let bytes = payload(&info, &[first, second]);

        let tiles = split_tiles(&bytes, &info).unwrap();
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0], first, "the first tile stops at its coded size");
        assert_eq!(tiles[1], second, "the last tile takes the remainder");
    }

    #[test]
    fn reads_a_tile_size_prefix_little_endian_across_bytes() {
        // `tile_size_minus_1` is `le(TileSizeBytes)` (§5.11.1, §4.10.4): 299 = 0x012b codes as
        // `2b 01`, so the second byte is worth 256. Every other fixture fits in its first byte,
        // where the shift by `8 * i` is a shift by zero.
        let mut info = two_column_grid();
        info.tile_size_bytes = 2;
        let first = vec![0x5a_u8; 300];
        let second: &[u8] = &[1, 2];
        let bytes = payload(&info, &[&first, second]);
        assert_eq!(&bytes[1..3], &[0x2b, 0x01]);

        let tiles = split_tiles(&bytes, &info).unwrap();
        assert_eq!(tiles[0].len(), 300);
        assert_eq!(tiles[1], second);
    }

    #[test]
    fn a_complete_size_field_with_no_tile_after_it_overruns_the_payload() {
        // The payload holds exactly the 4-byte size field and nothing else: the field itself is
        // whole, so what is wrong is the tile it promises, not the field.
        let info = two_column_grid();
        let bytes = [0u8, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: tile size runs past the end of the payload")
        );
    }

    #[test]
    fn a_single_tile_group_codes_no_flag_and_no_size() {
        let info = single_tile_grid();
        let only: &[u8] = &[0xaa, 0xbb, 0xcc];
        let tiles = split_tiles(only, &info).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], only);
    }

    #[test]
    fn rejects_a_size_prefix_that_overruns_the_payload() {
        let info = two_column_grid();
        // flag byte, then a size claiming 0x100 bytes with only a few following.
        let bytes = [0u8, 0xff, 0x00, 0x00, 0x00, 1, 2, 3];
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: tile size runs past the end of the payload")
        );
    }

    #[test]
    fn rejects_a_payload_that_ends_inside_a_size_field() {
        let info = two_column_grid();
        let bytes = [0u8, 0x01, 0x00]; // flag, then only 2 of the 4 size bytes
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: payload ends inside a tile size field")
        );
    }

    #[test]
    fn rejects_a_final_tile_with_no_bytes_left() {
        let info = two_column_grid();
        // The first tile consumes everything, leaving the last tile empty.
        let bytes = [0u8, 0x02, 0x00, 0x00, 0x00, 1, 2, 3];
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: empty tile")
        );
    }

    #[test]
    fn refuses_a_partial_tile_group() {
        // tile_start_and_end_present_flag = 1, then tg_start = 1, tg_end = 1 (1 bit each), which
        // covers only the second of two tiles.
        let info = two_column_grid();
        // tile_start_and_end_present_flag = 1, tg_start = 1 (1 bit), tg_end = 1 (1 bit), then
        // zero padding to the byte boundary.
        let header = 0b1110_0000u8;
        let bytes = [header, 1, 2, 3];
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: a still image must carry every tile in one tile group")
        );
    }

    #[test]
    fn rejects_a_tile_range_outside_the_grid() {
        let mut info = two_column_grid();
        // Four tiles so tg_start/tg_end are 2 bits each, and ask for tile 3..2 (end < start).
        info.tile_cols = 2;
        info.tile_rows = 2;
        info.tile_rows_log2 = 1;
        info.mi_row_starts = vec![0, 16, 24];
        // tile_start_and_end_present_flag = 1, tg_start = 3 (2 bits), tg_end = 2 (2 bits), then
        // zero padding.
        let header = 0b1111_0000u8;
        let bytes = [header, 1, 2, 3];
        assert_eq!(
            split_tiles(&bytes, &info).unwrap_err().static_message(),
            Some("AV1 tile group: tg_start/tg_end fall outside the tile grid")
        );
    }
}
