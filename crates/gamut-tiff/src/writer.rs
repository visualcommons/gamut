//! Codec-level serialisation: laying image (strip/tile) data out around an IFD.
//!
//! The structural serialisation — the byte-order header, the IFD chain, and the two-pass offset
//! layout of out-of-line values — is [`gamut_ifd::write`]. This module composes that primitive with
//! the codec's pixel data: it sizes the directory, places each strip/tile contiguously after it, and
//! fills in the `StripOffsets`/`StripByteCounts` (or `TileOffsets`/`TileByteCounts`) that point at
//! them. `variant` selects classic TIFF or BigTIFF throughout. The placeholder-then-patch layout
//! itself is [`gamut_ifd::write_with_blocks`], shared with gamut-dng.

use gamut_core::Result;
use gamut_ifd::{BlockRun, ByteOrder, Ifd, Variant, write_with_blocks};

/// Serialises a single-IFD strip TIFF image: the supplied directory plus its strip data,
/// referenced by `StripOffsets`/`StripByteCounts`. The strips are written verbatim, so the caller
/// is responsible for any per-strip compression. `variant` selects classic TIFF or BigTIFF.
///
/// # Errors
///
/// Returns an error if the stream is not representable in the selected variant's widths (see
/// [`gamut_ifd::write`]).
pub fn write_image(
    order: ByteOrder,
    variant: Variant,
    ifd: &Ifd,
    strips: &[Vec<u8>],
) -> Result<Vec<u8>> {
    write_blocks(order, variant, ifd, BlockRun::strips(strips))
}

/// Serialises a single-IFD tiled TIFF image: the directory plus its tile data, referenced by
/// `TileOffsets`/`TileByteCounts`. The caller supplies the `TileWidth`/`TileLength` fields and any
/// per-tile compression. `variant` selects classic TIFF or BigTIFF.
///
/// # Errors
///
/// Returns an error if the stream is not representable in the selected variant's widths (see
/// [`gamut_ifd::write`]).
pub fn write_image_tiled(
    order: ByteOrder,
    variant: Variant,
    ifd: &Ifd,
    tiles: &[Vec<u8>],
) -> Result<Vec<u8>> {
    write_blocks(order, variant, ifd, BlockRun::tiles(tiles))
}

/// Serialises a multi-page (multi-IFD) strip TIFF: each page's directory — linked through the
/// next-IFD pointers — plus all pages' strip data in a shared region after the directories. Each
/// page is `(directory, strips)`; `StripOffsets`/`StripByteCounts` are filled in here. `variant`
/// selects classic TIFF or BigTIFF.
///
/// # Errors
///
/// Returns an error if the stream is not representable in the selected variant's widths (see
/// [`gamut_ifd::write`]).
pub fn write_multipage(
    order: ByteOrder,
    variant: Variant,
    pages: &[(Ifd, Vec<Vec<u8>>)],
) -> Result<Vec<u8>> {
    // Every page's strips are one run, installed on that page's directory; all pages' data follows
    // the directories, page by page.
    let runs: Vec<BlockRun<'_>> = pages
        .iter()
        .map(|(_, strips)| BlockRun::strips(strips))
        .collect();
    write_with_blocks(order, variant, &runs, |placed| {
        pages
            .iter()
            .zip(placed)
            .map(|((ifd, _), run)| {
                let mut ifd = ifd.clone();
                run.install(&mut ifd);
                ifd
            })
            .collect()
    })
}

/// Lays out one directory plus one run of data blocks after it.
fn write_blocks(
    order: ByteOrder,
    variant: Variant,
    ifd: &Ifd,
    run: BlockRun<'_>,
) -> Result<Vec<u8>> {
    write_with_blocks(order, variant, &[run], |placed| {
        let mut ifd = ifd.clone();
        placed[0].install(&mut ifd);
        vec![ifd]
    })
}

#[cfg(test)]
mod tests {
    use gamut_ifd::Value;

    use super::*;
    use crate::tags;

    /// A directory whose only field is a 9-byte `XMP` packet: out of line in both variants (past
    /// classic's 4-byte and BigTIFF's 8-byte inline word) and odd, so the value pool ends on an
    /// odd byte and the strip data needs the zero pad byte to start on a word boundary.
    fn odd_pool_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::XMP, Value::Byte(b"abcdefghi".to_vec()));
        ifd
    }

    /// The classic layout `write_image` produces for [`odd_pool_ifd`] and one 3-byte strip.
    #[rustfmt::skip]
    const CLASSIC_ODD_POOL: &[u8] = &[
        b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD 0 at 8
        0x03, 0x00, // three entries
        // StripOffsets (273): LONG, 1, the strip at 60 — the even base after the 59-byte pool end.
        0x11, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00,
        // StripByteCounts (279): LONG, 1, 3.
        0x17, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
        // XMP (700): BYTE, 9, out of line at 50.
        0xBC, 0x02, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x32, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, // next IFD: none
        b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', // value pool, 50..59
        0x00, // zero pad byte to the even base
        0xAA, 0xBB, 0xCC, // the strip, at 60
    ];

    /// Classic TIFF: `StripByteCounts` is `LONG`, `StripOffsets` is `LONG`, and the strip starts
    /// at the even base behind a zero pad byte.
    #[test]
    fn write_image_classic_lays_strips_at_the_even_base_after_an_odd_pool() {
        let got = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &odd_pool_ifd(),
            &[vec![0xAA, 0xBB, 0xCC]],
        )
        .expect("write");
        assert_eq!(got, CLASSIC_ODD_POOL);
    }

    /// BigTIFF: `StripByteCounts` stays `LONG` while `StripOffsets` widens to `LONG8`; the strip
    /// again starts at the even base behind a zero pad byte.
    #[test]
    fn write_image_bigtiff_lays_strips_at_the_even_base_after_an_odd_pool() {
        let got = write_image(
            ByteOrder::LittleEndian,
            Variant::Big,
            &odd_pool_ifd(),
            &[vec![0xAA, 0xBB, 0xCC]],
        )
        .expect("write");
        #[rustfmt::skip]
        let want: &[u8] = &[
            // header: BigTIFF, 8-byte offsets, IFD 0 at 16
            b'I', b'I', 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0,
            0x03, 0, 0, 0, 0, 0, 0, 0, // three entries
            // StripOffsets (273): LONG8, 1, the strip at 102 — the even base after the pool end.
            0x11, 0x01, 0x10, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x66, 0, 0, 0, 0, 0, 0, 0,
            // StripByteCounts (279): LONG, 1, 3.
            0x17, 0x01, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x03, 0, 0, 0, 0, 0, 0, 0,
            // XMP (700): BYTE, 9, out of line at 92.
            0xBC, 0x02, 0x01, 0x00, 0x09, 0, 0, 0, 0, 0, 0, 0, 0x5C, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, // next IFD: none
            b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', // value pool, 92..101
            0x00, // zero pad byte to the even base
            0xAA, 0xBB, 0xCC, // the strip, at 102
        ];
        assert_eq!(got, want);
    }

    /// `write_multipage` carries its own copy of the layout (#1057 pins it before the move into
    /// `gamut-ifd`): one page comes out byte-identical to [`write_image`]'s.
    #[test]
    fn write_multipage_single_page_matches_the_write_image_layout() {
        let got = write_multipage(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &[(odd_pool_ifd(), vec![vec![0xAA, 0xBB, 0xCC]])],
        )
        .expect("write");
        assert_eq!(got, CLASSIC_ODD_POOL);
    }
}
