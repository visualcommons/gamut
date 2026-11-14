//! Image-data decoding, shared by the raw image and every sub-image: the strip or tile grid, the
//! per-chunk decompression and unpacking, tile assembly, and row/column de-interleaving.

use gamut_core::{Error, Result};
use gamut_ifd::{ByteOrder, ZeroRowsPerStrip, extent_slices, rows_per_strip};

use super::TrackedIfd;
use crate::values::{Compression, SampleFormat, dng_compression};
use crate::{bitpack, compression, tags};

/// How an IFD's image data is chunked: TIFF row-band strips, or the DNG 1.7 tile grid.
#[derive(Debug)]
pub(super) enum ChunkGrid {
    /// `StripOffsets`/`StripByteCounts` row bands of `rows_per_strip` rows (fewer for the last).
    Strips { rows_per_strip: usize },
    /// `TileOffsets`/`TileByteCounts` over a `TileWidth × TileLength` grid. Every stored tile is
    /// full-size — edge tiles carry padding that assembly crops (TIFF 6.0 §15).
    Tiles {
        tile_width: usize,
        tile_height: usize,
        across: usize,
        down: usize,
    },
}

/// Classifies the IFD's chunk layout: tiled when the tile tags are present, else strips.
pub(super) fn chunk_grid(ifd: &TrackedIfd, width: usize, height: usize) -> Result<ChunkGrid> {
    if ifd.get(tags::TILE_OFFSETS).is_some() || ifd.get(tags::TILE_WIDTH).is_some() {
        let tile_width = ifd.get_u32(tags::TILE_WIDTH).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: tiled IFD missing TileWidth")
        })? as usize;
        let tile_height = ifd.get_u32(tags::TILE_LENGTH).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: tiled IFD missing TileLength")
        })? as usize;
        if tile_width == 0 || tile_height == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: tile dimensions must be non-zero",
            ));
        }
        Ok(ChunkGrid::Tiles {
            tile_width,
            tile_height,
            across: width.div_ceil(tile_width),
            down: height.div_ceil(tile_height),
        })
    } else {
        // DNG 1.7.1 gives a zero `RowsPerStrip` no reading, so it is refused rather than read as
        // one strip the way gamut-tiff does.
        let rows_per_strip = rows_per_strip(
            ifd.get_u32(tags::ROWS_PER_STRIP),
            height,
            ZeroRowsPerStrip::Refuse,
            env!("CARGO_PKG_NAME"),
        )?;
        Ok(ChunkGrid::Strips { rows_per_strip })
    }
}

/// Decodes an image IFD's chunked pixel data (strips or tiles, any supported compression) into
/// `width * height * spp` unpacked u16 samples. Shared by the raw image and sub-image paths.
pub(super) fn decode_image_data(
    ifd: &TrackedIfd,
    data: &[u8],
    order: ByteOrder,
    width: u32,
    height: u32,
    spp: usize,
    bits: u16,
) -> Result<Vec<u16>> {
    let compression = dng_compression(ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16)
        .ok_or_else(|| Error::unsupported(env!("CARGO_PKG_NAME"), "DNG: unknown compression"))?;
    // Reject an undecodable scheme up front, so an empty chunk list cannot mask it.
    if !compression::is_decodable(compression) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet decodable",
        ));
    }

    // SampleFormat (339) defaults to unsigned integer, the only encoding whose code values the
    // u16 sample model represents. Anything else must fail cleanly, not silently misdecode.
    if let Some(formats) = ifd.get_u32_vec(tags::SAMPLE_FORMAT) {
        for format in formats {
            match u16::try_from(format)
                .ok()
                .and_then(|f| SampleFormat::try_from(f).ok())
            {
                Some(SampleFormat::UnsignedInteger) => {}
                Some(SampleFormat::FloatingPoint) => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "DNG: floating-point sample data is not supported",
                    ));
                }
                _ => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "DNG: only unsigned-integer samples are supported",
                    ));
                }
            }
        }
    }

    // PlanarConfiguration (284) defaults to chunky (1), the only layout the interleaved sample
    // model represents. Planar (2) is legal TIFF that a DNG must not use (the SDK's IsValidDNG
    // rejects it) and would otherwise be misread as chunky — silently wrong pixels.
    match ifd.get_u32(tags::PLANAR_CONFIGURATION) {
        None | Some(1) => {}
        Some(2) => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: planar component storage is not supported",
            ));
        }
        Some(_) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: PlanarConfiguration must be 1 (chunky) or 2 (planar)",
            ));
        }
    }

    let predictor = crate::predictor::validate(ifd.get_u32(tags::PREDICTOR), compression, bits)?;

    let (width, height) = (width as usize, height as usize);
    let samples_per_row = width
        .checked_mul(spp)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;
    let expected = samples_per_row
        .checked_mul(height)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;

    let row_factor = interleave_factor(ifd, tags::ROW_INTERLEAVE_FACTOR, height)?;
    let col_factor = interleave_factor(ifd, tags::COLUMN_INTERLEAVE_FACTOR, width)?;

    let layout = ChunkLayout {
        compression,
        spp,
        bits,
        order,
        predictor,
    };
    let grid = chunk_grid(ifd, width, height)?;
    let chunks = grid_chunks(ifd, data, &grid)?;
    // The output buffer is never reserved from the declared geometry. `ImageWidth`/`ImageLength`
    // are two tag values a few-hundred-byte file can set to 60000 each (#564), so the buffer
    // grows only by samples a chunk has already decoded from bytes the file actually holds. That
    // bounds every allocation here by the data rather than by the tags, without a fixed size cap
    // that would refuse a genuine large raw; growth is amortised by `Vec`'s doubling.
    let samples = match grid {
        // Each strip is an independent sample stream of full-width rows; strips concatenate as
        // row bands. Decoding works strip by strip — concatenating packed bytes first would
        // misalign any sub-byte strip whose packed length is not what whole-image geometry
        // predicts (rows are byte-aligned *per strip*).
        ChunkGrid::Strips { rows_per_strip } => {
            let mut samples = Vec::new();
            let mut remaining_rows = height;
            for chunk in &chunks {
                let rows = rows_per_strip.min(remaining_rows);
                if rows == 0 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: more strips than image rows",
                    ));
                }
                samples.extend(decode_chunk_samples(layout, chunk, width, rows)?);
                remaining_rows -= rows;
            }
            if samples.len() != expected {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: raw image data is truncated",
                ));
            }
            samples
        }
        // Tiles decode at full tile size, one row of tiles at a time; that band's image rows are
        // then appended left to right, cropping the edge tiles' padding.
        ChunkGrid::Tiles {
            tile_width,
            tile_height,
            across,
            down,
        } => {
            let tile_count = across.checked_mul(down).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow")
            })?;
            if chunks.len() != tile_count {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: tile count must cover the image grid",
                ));
            }
            let mut samples = Vec::new();
            // The bands are walked over the chunks the file holds, not counted over `0..down`: a
            // zero `ImageWidth` reaches here (the raw path checks `Dimensions` afterwards) with
            // `across == 0` and no chunks, while `ImageLength = u32::MAX` over `TileLength = 1`
            // still declares ~4.29e9 bands, so a counted loop would make that many empty passes.
            // `max(1)` only keeps `chunks` (which rejects 0) callable on that empty slice. This
            // replaces index arithmetic (`band * across` slicing) and its operator mutants: the
            // tile-count check above is what pins one band to `across` chunks.
            for (band, band_chunks) in chunks.chunks(across.max(1)).enumerate() {
                let tiles = band_chunks
                    .iter()
                    .map(|chunk| decode_chunk_samples(layout, chunk, tile_width, tile_height))
                    .collect::<Result<Vec<_>>>()?;
                let band_rows = tile_height.min(height - band * tile_height);
                for r in 0..band_rows {
                    for (i, tile) in tiles.iter().enumerate() {
                        let copy_cols = tile_width.min(width - i * tile_width);
                        let src = r * tile_width * spp;
                        samples.extend_from_slice(&tile[src..src + copy_cols * spp]);
                    }
                }
            }
            samples
        }
    };

    // Row/column interleaving (RowInterleaveFactor 50975 / ColumnInterleaveFactor 52547): the
    // stored image concatenates the interleave fields; the logical image re-interleaves them.
    // Applied to the whole assembled image, matching the SDK (dng_read_image::Read reads a full
    // temporary image, then runs Interleave2D over it).
    if row_factor > 1 || col_factor > 1 {
        Ok(deinterleave(
            &samples, width, height, spp, row_factor, col_factor,
        ))
    } else {
        Ok(samples)
    }
}

/// Reads a `RowInterleaveFactor`/`ColumnInterleaveFactor` tag, validating it against the image
/// extent (the SDK rejects factors of 0 or beyond the axis length).
fn interleave_factor(ifd: &TrackedIfd, tag: u16, limit: usize) -> Result<usize> {
    match ifd.get_u32(tag) {
        None => Ok(1),
        Some(f) => {
            let f = f as usize;
            if f == 0 || f > limit {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: interleave factor out of valid range",
                ));
            }
            Ok(f)
        }
    }
}

/// Re-interleaves a stored field-concatenated image into logical pixel order (the decode
/// direction of the SDK's `Interleave2D`).
///
/// With factors `(rf, cf)`, the stored image stacks `rf` row fields (field `i` holds logical
/// rows `r` with `r % rf == i`) and, within each, `cf` column fields likewise; field `i` gets
/// `len / factor` rows/columns, with the first `len % factor` fields one longer. The logical
/// pixel `(r, c)` therefore lives at stored row `offset(r % rf) + r / rf`, column
/// `offset(c % cf) + c / cf`.
fn deinterleave(
    samples: &[u16],
    width: usize,
    height: usize,
    spp: usize,
    row_factor: usize,
    col_factor: usize,
) -> Vec<u16> {
    let field_offset = |index: usize, len: usize, factor: usize| -> usize {
        index * (len / factor) + index.min(len % factor)
    };
    let mut out = vec![0u16; samples.len()];
    for r in 0..height {
        let src_r = field_offset(r % row_factor, height, row_factor) + r / row_factor;
        for c in 0..width {
            let src_c = field_offset(c % col_factor, width, col_factor) + c / col_factor;
            let src = (src_r * width + src_c) * spp;
            let dst = (r * width + c) * spp;
            out[dst..dst + spp].copy_from_slice(&samples[src..src + spp]);
        }
    }
    out
}

/// How one chunk's samples are stored: everything but the bytes themselves. Bundled so the
/// per-chunk decode takes a layout rather than a long positional argument list.
#[derive(Debug, Clone, Copy)]
struct ChunkLayout {
    /// The chunk's compression scheme.
    compression: Compression,
    /// Samples per pixel.
    spp: usize,
    /// Bits per sample.
    bits: u16,
    /// The stream's byte order.
    order: ByteOrder,
    /// The `Predictor` to undo after unpacking.
    predictor: crate::values::Predictor,
}

/// Decodes one chunk (a strip or tile) of `cols x rows` pixels, returning exactly
/// `cols * rows * layout.spp` samples.
fn decode_chunk_samples(
    layout: ChunkLayout,
    chunk: &[u8],
    cols: usize,
    rows: usize,
) -> Result<Vec<u16>> {
    let ChunkLayout {
        compression,
        spp,
        bits,
        order,
        predictor,
    } = layout;
    let want = cols
        .checked_mul(rows)
        .and_then(|n| n.checked_mul(spp))
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;
    match compression {
        Compression::None | Compression::Deflate => {
            // Cap inflation at the packed length this chunk's geometry implies: a Deflate chunk
            // that expands past it cannot be a valid sample stream, and the cap is what keeps a
            // hostile zlib stream from allocating without bound.
            let max_out = cols
                .checked_mul(spp)
                .and_then(|per_row| bitpack::packed_len(per_row, bits, rows))
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow")
                })?;
            let bytes = compression::decompress(compression, chunk, max_out)?;
            // Short data is refused *before* unpacking, because the unpacker reserves the
            // geometry's full `cols * rows * spp` samples: checked afterwards, a chunk whose tags
            // claim 60000 x 60000 pixels over a few hundred bytes would already have asked the
            // allocator for gigabytes (#564). The bytes in hand are what bound the allocation.
            // Every byte width unpacks at least `want` samples from `max_out` bytes, so this is
            // the same acceptance set as comparing the unpacked count against `want`.
            if bytes.len() < max_out {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: raw image data is truncated",
                ));
            }
            let mut got = bitpack::unpack(&bytes, bits, cols * spp, rows, order);
            got.truncate(want); // tolerate chunk padding, per TIFF practice
            // The predictor is undone per chunk, after unpacking and (inside `bitpack`) after the
            // byte-order swap — the SDK's ordering.
            crate::predictor::undo(predictor, &mut got, cols, rows, spp, bits);
            Ok(got)
        }
        // Lossless JPEG decodes samples directly. The JPEG stream's internal width/height/
        // components need not match the chunk's geometry — only the total sample count must
        // (DNG 1.7.1, "Compression": real CFA writers store a two-component stream at half
        // width).
        Compression::Jpeg => {
            let jpeg = gamut_jpeg::lossless::decode(chunk)?;
            if jpeg.samples.len() != want {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: lossless-JPEG sample count mismatch",
                ));
            }
            Ok(jpeg.samples)
        }
        // JPEG XL (DNG 1.7): each chunk is a complete bitstream whose geometry/channels must
        // agree with the layout (validated inside the bridge); output is full-range 16-bit,
        // matching the reference SDK — `bits` describes only the codestream's stored precision.
        Compression::JpegXl => crate::jxl::decode_chunk(chunk, cols, rows, spp),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet decodable",
        )),
    }
}

/// Returns the grid's chunks as raw byte slices, in offset-array order. Offsets and counts are
/// read at full 64-bit width (BigTIFF writes them as `LONG8`).
pub(super) fn grid_chunks<'a>(
    ifd: &TrackedIfd,
    data: &'a [u8],
    grid: &ChunkGrid,
) -> Result<Vec<&'a [u8]>> {
    let (offset_tag, count_tag, missing) = match grid {
        ChunkGrid::Strips { .. } => (
            tags::STRIP_OFFSETS,
            tags::STRIP_BYTE_COUNTS,
            "DNG: missing StripOffsets/StripByteCounts",
        ),
        ChunkGrid::Tiles { .. } => (
            tags::TILE_OFFSETS,
            tags::TILE_BYTE_COUNTS,
            "DNG: missing TileOffsets/TileByteCounts",
        ),
    };
    let offsets = ifd
        .get_u64_vec(offset_tag)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), missing))?;
    let counts = ifd
        .get_u64_vec(count_tag)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), missing))?;
    extent_slices(&offsets, &counts, data, env!("CARGO_PKG_NAME"))
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{Ifd, Value};

    use super::*;

    #[test]
    fn chunk_grid_rejects_zero_tile_dimensions() {
        for (tw, th) in [(0u16, 2u16), (2, 0)] {
            let mut ifd = Ifd::new();
            ifd.set(tags::TILE_WIDTH, Value::Short(vec![tw]));
            ifd.set(tags::TILE_LENGTH, Value::Short(vec![th]));
            ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
            ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4]));
            let err = chunk_grid(&TrackedIfd::new(&ifd), 4, 4).unwrap_err();
            assert!(
                matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("non-zero"))),
                "({tw}, {th}): {err:?}"
            );
        }
    }

    /// A strip IFD's `RowsPerStrip` of 0 is refused, unlike `gamut-tiff`, which reads it as one
    /// full-height strip; an absent tag still means one strip of the whole height. This pins the
    /// policy `chunk_grid` passes to `gamut_ifd::rows_per_strip`, whose own tests pin the rest;
    /// the extent checks the chunks then pass through are `gamut_ifd::extent_slices`'s, pinned
    /// there.
    #[test]
    fn chunk_grid_refuses_zero_rows_per_strip_and_defaults_an_absent_one_to_the_height() {
        let mut ifd = Ifd::new();
        ifd.set(tags::ROWS_PER_STRIP, Value::Long(vec![0]));
        let err = chunk_grid(&TrackedIfd::new(&ifd), 4, 3).unwrap_err();
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput, "{err:?}");
        assert_eq!(err.static_message(), Some("RowsPerStrip must be non-zero"));
        assert_eq!(err.origin(), Some(env!("CARGO_PKG_NAME")), "{err:?}");

        let grid = chunk_grid(&TrackedIfd::new(&Ifd::new()), 4, 3).expect("strips");
        assert!(
            matches!(grid, ChunkGrid::Strips { rows_per_strip: 3 }),
            "{grid:?}"
        );
    }

    /// Either tile tag alone classifies the IFD as tiled — the resulting error must come from
    /// the *tile* family, not from a strips fallback complaining about StripOffsets.
    #[test]
    fn either_tile_tag_alone_classifies_as_tiled() {
        // Geometry present, offsets missing.
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![16]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![16]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[],
            ByteOrder::LittleEndian,
            16,
            16,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("Tile"))),
            "{err:?}"
        );
        // Offsets present, geometry missing.
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0; 4],
            ByteOrder::LittleEndian,
            16,
            16,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("TileWidth"))),
            "{err:?}"
        );
    }

    /// Height 4 at RowsPerStrip 2 needs exactly two strips; a third must fail with the
    /// strip-count error (not a generic truncation), pinning the remaining-rows bookkeeping.
    #[test]
    fn decode_image_data_rejects_surplus_strips() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![2]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0, 8, 16]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![8, 8, 8]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0u8; 24],
            ByteOrder::LittleEndian,
            4,
            4,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("more strips"))),
            "{err:?}"
        );
    }

    /// Single-axis interleaving (the Adobe Bayer sample sets *both* factors, so it cannot
    /// distinguish the two conditions): each factor alone must trigger the de-interleave.
    #[test]
    fn single_axis_interleave_deinterleaves() {
        let image = |tag: u16, w: u16, h: u16, data: &[u8]| {
            let mut ifd = Ifd::new();
            ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
            ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![h]));
            ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
            ifd.set(
                tags::STRIP_BYTE_COUNTS,
                Value::Long(vec![u32::from(w) * u32::from(h)]),
            );
            ifd.set(tag, Value::Short(vec![2]));
            decode_image_data(
                &TrackedIfd::new(&ifd),
                data,
                ByteOrder::LittleEndian,
                u32::from(w),
                u32::from(h),
                1,
                8,
            )
            .expect("decode")
        };
        // Row-only: a 1-wide, 4-tall image stores its rows as [r0, r2 | r1, r3].
        assert_eq!(
            image(tags::ROW_INTERLEAVE_FACTOR, 1, 4, &[10, 30, 20, 40]),
            vec![10, 20, 30, 40]
        );
        // Column-only: a 4-wide, 1-tall image stores its columns as [c0, c2 | c1, c3].
        assert_eq!(
            image(tags::COLUMN_INTERLEAVE_FACTOR, 4, 1, &[10, 30, 20, 40]),
            vec![10, 20, 30, 40]
        );
    }

    /// Interleaved *multi-plane* data moves whole pixels: the per-pixel sample pairs stay
    /// together under the column shuffle (a `spp` slip in the flat indexing would tear them).
    #[test]
    fn interleave_moves_whole_pixels_across_planes() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![2]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![8]));
        ifd.set(tags::COLUMN_INTERLEAVE_FACTOR, Value::Short(vec![2]));
        // Stored pixel pairs in field order [c0, c2 | c1, c3], two samples each.
        let stored = [10u8, 11, 30, 31, 20, 21, 40, 41];
        let got = decode_image_data(
            &TrackedIfd::new(&ifd),
            &stored,
            ByteOrder::LittleEndian,
            4,
            1,
            2,
            8,
        )
        .expect("decode");
        assert_eq!(got, vec![10, 11, 20, 21, 30, 31, 40, 41]);
    }

    /// A factor equal to the axis length is valid (the SDK allows `1..=axis`); one past it is
    /// not. Factor-per-column de-interleaving is the identity, which pins the boundary exactly.
    #[test]
    fn interleave_factor_accepts_the_full_axis() {
        let decode_with_factor = |factor: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
            ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
            ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
            ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![4]));
            ifd.set(tags::COLUMN_INTERLEAVE_FACTOR, Value::Short(vec![factor]));
            decode_image_data(
                &TrackedIfd::new(&ifd),
                &[1, 2, 3, 4],
                ByteOrder::LittleEndian,
                4,
                1,
                1,
                8,
            )
        };
        assert_eq!(
            decode_with_factor(4).expect("factor == width"),
            vec![1, 2, 3, 4]
        );
        assert!(decode_with_factor(5).is_err());
    }

    /// A truncated tile is a typed error before assembly could index out of range.
    #[test]
    fn truncated_tile_data_is_a_typed_error_not_a_panic() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![2])); // 2 of the 4 needed bytes
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0u8; 2],
            ByteOrder::LittleEndian,
            2,
            2,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("truncated"))),
            "{err:?}"
        );
    }

    /// Hand-computed golden for the SDK's `Interleave2D` decode mapping: fields are concatenated
    /// in storage, with the first `len % factor` fields one row/column longer.
    #[test]
    fn deinterleave_reassembles_fields_row_major() {
        // 4x4, both factors 2: stored rows are [row-field 0 | row-field 1], each with column
        // fields likewise. The stored image below maps back to the logical 0..16 raster.
        let stored = [
            0u16, 2, 1, 3, //
            8, 10, 9, 11, //
            4, 6, 5, 7, //
            12, 14, 13, 15,
        ];
        let logical: Vec<u16> = (0..16).collect();
        assert_eq!(deinterleave(&stored, 4, 4, 1, 2, 2), logical);

        // Uneven split: width 3, column factor 2 — field 0 gets two columns, field 1 one.
        assert_eq!(deinterleave(&[10, 30, 20], 3, 1, 1, 1, 2), vec![10, 20, 30]);

        // Factor 1 on both axes is the identity.
        assert_eq!(deinterleave(&[1, 2, 3, 4], 2, 2, 1, 1, 1), vec![1, 2, 3, 4]);
    }

    #[test]
    fn interleave_factor_validates_range() {
        let mut ifd = Ifd::new();
        assert_eq!(
            interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).unwrap(),
            1
        );
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![2]));
        assert_eq!(
            interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).unwrap(),
            2
        );
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![0]));
        assert!(interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).is_err());
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![9]));
        assert!(interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).is_err());
    }

    /// Hand-computed golden for tile reassembly (the counterpart of the encoder's splitter
    /// golden): a 3x3 image from a 2x2 grid of 2x2 zero-padded tiles, plus the count guard.
    #[test]
    fn decode_image_data_assembles_tiles_with_edge_crop() {
        let tiles: [&[u8]; 4] = [&[1, 2, 4, 5], &[3, 0, 6, 0], &[7, 8, 0, 0], &[9, 0, 0, 0]];
        let data: Vec<u8> = tiles.concat();
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0, 4, 8, 12]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4; 4]));

        let samples = decode_image_data(
            &TrackedIfd::new(&ifd),
            &data,
            ByteOrder::LittleEndian,
            3,
            3,
            1,
            8,
        )
        .expect("decode");
        assert_eq!(samples, (1..=9).collect::<Vec<u16>>());

        // A tile list that does not cover the grid is rejected.
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0, 4, 8]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4; 3]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &data,
            ByteOrder::LittleEndian,
            3,
            3,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("tile count"))),
            "expected a tile-count error, got {err:?}"
        );

        // A tiled IFD missing its geometry is rejected.
        ifd.remove(tags::TILE_WIDTH);
        assert!(
            decode_image_data(
                &TrackedIfd::new(&ifd),
                &data,
                ByteOrder::LittleEndian,
                3,
                3,
                1,
                8
            )
            .is_err()
        );
    }
}
