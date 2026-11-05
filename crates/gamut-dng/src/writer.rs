//! Codec-level serialisation: laying the preview and raw image data out around the DNG's IFD tree.
//!
//! The structural layout — the byte-order header, the IFD *tree* (IFD 0 plus its raw sub-IFD), and
//! the two-pass offset layout of out-of-line values — is [`gamut_ifd::write`]. This module composes
//! that primitive with the codec's pixel data: it writes the directory tree once with correctly
//! sized placeholder offsets to learn where the value pool ends, places each image's strips after
//! it, fills in the `StripOffsets`/`StripByteCounts` (or tile equivalents), and writes again. The
//! re-write is byte-stable because only the *values* of the offset fields change, not their sizes.
//! That layout is [`gamut_ifd::write_with_blocks`], shared with gamut-tiff.

use gamut_core::Result;
use gamut_ifd::{BlockRun, ByteOrder, Ifd, Variant, write_with_blocks};

use crate::tags;

/// One image's data blocks (strips or tiles) plus the tags that reference them.
pub(crate) struct ImageBlocks {
    /// The offset tag — [`tags::STRIP_OFFSETS`] or [`tags::TILE_OFFSETS`].
    pub offset_tag: u16,
    /// The byte-count tag — [`tags::STRIP_BYTE_COUNTS`] or [`tags::TILE_BYTE_COUNTS`].
    pub bytecount_tag: u16,
    /// The data blocks (already compressed, if applicable), in file order.
    pub blocks: Vec<Vec<u8>>,
}

impl ImageBlocks {
    /// The blocks as the run [`write_with_blocks`] lays out.
    fn run(&self) -> BlockRun<'_> {
        BlockRun::new(self.offset_tag, self.bytecount_tag, &self.blocks)
    }
}

/// Serialises a DNG whose IFD 0 carries `preview` image data and whose single raw sub-IFD
/// (`SubIFDs`, [`tags::SUB_IFDS`]) carries `raw` image data.
///
/// `ifd0` and `raw_ifd` supply every field *except* the offset/byte-count pair, which this fills
/// in. The preview data is laid out first, then the raw data, both after the directory tree.
pub(crate) fn write_cfa_dng(
    order: ByteOrder,
    variant: Variant,
    ifd0: Ifd,
    preview: &ImageBlocks,
    raw_ifd: Ifd,
    raw: &ImageBlocks,
) -> Result<Vec<u8>> {
    // The preview strips are laid out first, then the raw strips; the raw run is installed on the
    // sub-IFD that references it.
    write_with_blocks(order, variant, &[preview.run(), raw.run()], |placed| {
        let mut ifd0 = ifd0.clone();
        placed[0].install(&mut ifd0);
        let mut raw_ifd = raw_ifd.clone();
        placed[1].install(&mut raw_ifd);
        ifd0.set_sub_ifd(tags::SUB_IFDS, vec![raw_ifd]);
        vec![ifd0]
    })
}

#[cfg(test)]
mod tests {
    use gamut_ifd::Value;

    use super::*;

    /// A one-strip image's blocks under the strip tags.
    fn strips(block: &[u8]) -> ImageBlocks {
        ImageBlocks {
            offset_tag: tags::STRIP_OFFSETS,
            bytecount_tag: tags::STRIP_BYTE_COUNTS,
            blocks: vec![block.to_vec()],
        }
    }

    /// Writes IFD 0 carrying a 9-byte `XMP` packet — out of line in both variants (past classic's
    /// 4-byte and BigTIFF's 8-byte inline word) and odd, so the value pool ends on an odd byte —
    /// with a one-byte preview strip, and an empty raw sub-IFD with a two-byte strip.
    fn write_odd_pool(variant: Variant) -> Vec<u8> {
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::XMP, Value::Byte(b"abcdefghi".to_vec()));
        write_cfa_dng(
            ByteOrder::LittleEndian,
            variant,
            ifd0,
            &strips(&[0x11]),
            Ifd::new(),
            &strips(&[0x22, 0x33]),
        )
        .expect("write")
    }

    /// Classic: both images' `StripByteCounts` and `StripOffsets` are `LONG`; the preview strip
    /// starts at the even base behind a zero pad byte and the raw strip follows it directly.
    #[test]
    fn write_cfa_dng_classic_lays_blocks_at_the_even_base_after_an_odd_pool() {
        #[rustfmt::skip]
        let want: &[u8] = &[
            b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD 0 at 8
            0x04, 0x00, // IFD 0: four entries
            // StripOffsets (273): LONG, 1, the preview strip at 102 — the even base.
            0x11, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x66, 0x00, 0x00, 0x00,
            // StripByteCounts (279): LONG, 1, 1.
            0x17, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            // SubIFDs (330): LONG, 1, the raw IFD at 62.
            0x4A, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00,
            // XMP (700): BYTE, 9, out of line at 92.
            0xBC, 0x02, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x5C, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // next IFD: none
            0x02, 0x00, // raw IFD at 62: two entries
            // StripOffsets (273): LONG, 1, the raw strip at 103, right after the preview's.
            0x11, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x67, 0x00, 0x00, 0x00,
            // StripByteCounts (279): LONG, 1, 2.
            0x17, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // next IFD: none
            b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', // value pool, 92..101
            0x00, // zero pad byte to the even base
            0x11, // preview strip, at 102
            0x22, 0x33, // raw strip, at 103
        ];
        assert_eq!(write_odd_pool(Variant::Classic), want);
    }

    /// BigTIFF: `StripByteCounts` stays `LONG` while `StripOffsets` and the `SubIFDs` pointer
    /// widen to `LONG8`; the blocks again start at the even base behind a zero pad byte.
    #[test]
    fn write_cfa_dng_bigtiff_lays_blocks_at_the_even_base_after_an_odd_pool() {
        #[rustfmt::skip]
        let want: &[u8] = &[
            // header: BigTIFF, 8-byte offsets, IFD 0 at 16
            b'I', b'I', 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0,
            0x04, 0, 0, 0, 0, 0, 0, 0, // IFD 0: four entries
            // StripOffsets (273): LONG8, 1, the preview strip at 178 — the even base.
            0x11, 0x01, 0x10, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0xB2, 0, 0, 0, 0, 0, 0, 0,
            // StripByteCounts (279): LONG, 1, 1.
            0x17, 0x01, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0,
            // SubIFDs (330): LONG8, 1, the raw IFD at 112.
            0x4A, 0x01, 0x10, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x70, 0, 0, 0, 0, 0, 0, 0,
            // XMP (700): BYTE, 9, out of line at 168.
            0xBC, 0x02, 0x01, 0x00, 0x09, 0, 0, 0, 0, 0, 0, 0, 0xA8, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, // next IFD: none
            0x02, 0, 0, 0, 0, 0, 0, 0, // raw IFD at 112: two entries
            // StripOffsets (273): LONG8, 1, the raw strip at 179, right after the preview's.
            0x11, 0x01, 0x10, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0xB3, 0, 0, 0, 0, 0, 0, 0,
            // StripByteCounts (279): LONG, 1, 2.
            0x17, 0x01, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, // next IFD: none
            b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', // value pool, 168..177
            0x00, // zero pad byte to the even base
            0x11, // preview strip, at 178
            0x22, 0x33, // raw strip, at 179
        ];
        assert_eq!(write_odd_pool(Variant::Big), want);
    }
}
