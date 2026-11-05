//! Laying image data out after a directory tree: the placeholder-then-patch layout every
//! TIFF-family encoder needs once it has strips or tiles to store.
//!
//! An encoder knows its data blocks before it knows where they land: the blocks follow the header,
//! the directories and the value pool, whose length depends on the directories — which must name
//! the blocks' offsets. [`write_with_blocks`] closes the loop once. It builds the directories with
//! correctly sized and typed placeholder offsets (so the layout is already final; only offset
//! *values* change afterwards), writes them to learn where the value pool ends, places every run
//! of blocks contiguously from the next word boundary, and writes again with the real offsets.

use gamut_core::{Error, Result};

use crate::writer::write;
use crate::{ByteOrder, Ifd, TiffFile, Value, Variant, align_word};

/// One run of data blocks — an image's strips or tiles — and the offset/byte-count tag pair its
/// directory references them by.
#[derive(Debug, Clone, Copy)]
pub struct BlockRun<'a> {
    offset_tag: u16,
    count_tag: u16,
    blocks: &'a [Vec<u8>],
}

impl<'a> BlockRun<'a> {
    /// `blocks`, referenced by `offset_tag` (e.g. `StripOffsets`) and `count_tag` (e.g.
    /// `StripByteCounts`), stored in file order.
    #[must_use]
    pub fn new(offset_tag: u16, count_tag: u16, blocks: &'a [Vec<u8>]) -> Self {
        Self {
            offset_tag,
            count_tag,
            blocks,
        }
    }

    /// Strips, referenced by `StripOffsets` (273) and `StripByteCounts` (279).
    #[must_use]
    pub fn strips(blocks: &'a [Vec<u8>]) -> Self {
        Self::new(
            crate::tags::STRIP_OFFSETS,
            crate::tags::STRIP_BYTE_COUNTS,
            blocks,
        )
    }

    /// Tiles, referenced by `TileOffsets` (324) and `TileByteCounts` (325).
    #[must_use]
    pub fn tiles(blocks: &'a [Vec<u8>]) -> Self {
        Self::new(
            crate::tags::TILE_OFFSETS,
            crate::tags::TILE_BYTE_COUNTS,
            blocks,
        )
    }
}

/// Where one [`BlockRun`] landed: the two fields its directory carries to reference it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedRun {
    offset_tag: u16,
    count_tag: u16,
    offsets: Value,
    counts: Value,
}

impl PlacedRun {
    /// Sets the run's offset and byte-count fields on `ifd`.
    pub fn install(&self, ifd: &mut Ifd) {
        ifd.set(self.count_tag, self.counts.clone());
        ifd.set(self.offset_tag, self.offsets.clone());
    }

    /// The offset field's value: a `LONG` array in a classic file, `LONG8` in a BigTIFF.
    #[must_use]
    pub fn offsets(&self) -> &Value {
        &self.offsets
    }

    /// The byte-count field's value: a `LONG` array.
    #[must_use]
    pub fn counts(&self) -> &Value {
        &self.counts
    }
}

/// Places `runs` against the offsets `starts` yields, one per block in run order.
fn place(
    variant: Variant,
    runs: &[BlockRun<'_>],
    mut starts: impl FnMut(usize) -> u64,
) -> Result<Vec<PlacedRun>> {
    runs.iter()
        .map(|run| {
            let offsets: Vec<u64> = run.blocks.iter().map(|b| starts(b.len())).collect();
            Ok(PlacedRun {
                offset_tag: run.offset_tag,
                count_tag: run.count_tag,
                offsets: Value::offset_array(variant, &offsets)?,
                // A `LONG` per block, whatever the variant: the byte counts' width is not the
                // offsets' (TIFF 6.0 §3 types them `SHORT` or `LONG`).
                counts: Value::Long(
                    run.blocks
                        .iter()
                        .map(|b| byte_count(b.len()))
                        .collect::<Result<_>>()?,
                ),
            })
        })
        .collect()
}

/// A block's length as the `LONG` its byte-count field stores, refused rather than truncated
/// when the block is 4 GiB or longer.
fn byte_count(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: block length exceeds the 4 GiB a LONG byte count holds",
        )
    })
}

/// Serialises the directories `tree` builds, followed by the data of every run in `runs`.
///
/// `tree` receives one [`PlacedRun`] per entry of `runs`, in the same order, and returns the
/// top-level directory chain with each run [installed](PlacedRun::install) on the directory —
/// top-level or sub-IFD — that references it. It is called twice: first with placeholder offsets
/// of the final width (so the layout it produces is already the final one), then with the real
/// offsets. It must therefore build the same tree both times.
///
/// The blocks are stored contiguously, in run order and then block order, from the first word
/// boundary after the directories' value pool; the gap, when the pool ends odd, is one zero byte.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when
/// [`write`](crate::write()) refuses the tree, when a classic-TIFF file would place a block past
/// the 4 GiB its 32-bit offsets address, or when any block is 4 GiB or longer — in a BigTIFF too,
/// because the byte counts are stored as `LONG` in both variants.
pub fn write_with_blocks(
    order: ByteOrder,
    variant: Variant,
    runs: &[BlockRun<'_>],
    mut tree: impl FnMut(&[PlacedRun]) -> Vec<Ifd>,
) -> Result<Vec<u8>> {
    let mut serialise = |placed: &[PlacedRun]| {
        write(&TiffFile {
            order,
            variant,
            ifds: tree(placed),
        })
    };
    let placeholders = place(variant, runs, |_| 0)?;
    let base = align_word(serialise(&placeholders)?.len() as u64);
    let mut cursor = base;
    let placed = place(variant, runs, |len| {
        let start = cursor;
        cursor += len as u64;
        start
    })?;
    let mut out = serialise(&placed)?;
    // `base` is the length just written, rounded up to even, and the placed tree has the same
    // length, so this appends the one alignment byte when the pool ended odd and nothing else.
    out.resize(base as usize, 0);
    for run in runs {
        for block in run.blocks {
            out.extend_from_slice(block);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{read, tags};

    /// A directory whose value pool ends odd: a 7-byte ASCII string (`"abcdef\0"`) out of line.
    fn odd_pool_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Long(vec![1])); // ImageWidth
        ifd.set(270, Value::Ascii("abcdef".to_owned()));
        ifd
    }

    /// Two runs in one directory: offsets are contiguous from the word-aligned base, in run order
    /// then block order, and the blocks follow the single alignment byte.
    #[test]
    fn runs_are_placed_contiguously_from_the_aligned_base() {
        let strips = [vec![1u8, 2, 3], vec![4]];
        let tiles = [vec![5u8, 6]];
        let runs = [BlockRun::strips(&strips), BlockRun::tiles(&tiles)];
        let out = write_with_blocks(ByteOrder::LittleEndian, Variant::Classic, &runs, |placed| {
            let mut ifd = odd_pool_ifd();
            for run in placed {
                run.install(&mut ifd);
            }
            vec![ifd]
        })
        .expect("write");

        let file = read(&out).expect("read");
        let ifd = &file.ifds[0];
        let base = ifd.get_u64_vec(tags::STRIP_OFFSETS).expect("strips")[0];
        assert_eq!(base % 2, 0, "the data starts on a word boundary");
        assert_eq!(
            ifd.get_u64_vec(tags::STRIP_OFFSETS),
            Some(vec![base, base + 3])
        );
        assert_eq!(ifd.get_u64_vec(tags::TILE_OFFSETS), Some(vec![base + 4]));
        assert_eq!(ifd.get_u64_vec(tags::STRIP_BYTE_COUNTS), Some(vec![3, 1]));
        assert_eq!(ifd.get(tags::TILE_BYTE_COUNTS), Some(&Value::Long(vec![2])));
        let base = base as usize;
        assert_eq!(&out[base..], [1, 2, 3, 4, 5, 6]);
        assert_eq!(out[base - 1], 0, "the alignment byte is zero");
    }

    /// The pool this directory leaves ends odd, so exactly one alignment byte separates it from
    /// the data: the base is the directory-only length rounded up, not further.
    #[test]
    fn the_base_is_the_directory_length_rounded_up_to_even() {
        let strips = [vec![9u8]];
        let runs = [BlockRun::strips(&strips)];
        let build = |placed: &[PlacedRun]| {
            let mut ifd = odd_pool_ifd();
            placed[0].install(&mut ifd);
            vec![ifd]
        };
        let out =
            write_with_blocks(ByteOrder::BigEndian, Variant::Classic, &runs, build).expect("write");
        let placeholders = place(Variant::Classic, &runs, |_| 0).expect("place");
        let directories = write(&TiffFile {
            order: ByteOrder::BigEndian,
            variant: Variant::Classic,
            ifds: build(&placeholders),
        })
        .expect("directories")
        .len();
        assert_eq!(directories % 2, 1, "the fixture's pool ends odd");
        assert_eq!(out.len(), directories + 1 + 1);
        assert_eq!(out.last(), Some(&9));
    }

    /// A run may sit on a sub-IFD: `tree` installs it there, and the data still lands after the
    /// whole tree.
    #[test]
    fn a_run_installed_on_a_sub_ifd_is_placed_after_the_tree() {
        let preview = [vec![1u8, 1]];
        let raw = [vec![2u8, 2, 2, 2]];
        let runs = [BlockRun::strips(&preview), BlockRun::strips(&raw)];
        let out = write_with_blocks(ByteOrder::LittleEndian, Variant::Classic, &runs, |placed| {
            let mut ifd0 = Ifd::new();
            placed[0].install(&mut ifd0);
            let mut child = Ifd::new();
            placed[1].install(&mut child);
            ifd0.set_sub_ifd(tags::SUB_IFDS, vec![child]);
            vec![ifd0]
        })
        .expect("write");
        let tree = crate::IfdReader::open(&out[..])
            .and_then(|mut reader| reader.read_tree(&[tags::SUB_IFDS]))
            .expect("tree");
        let ifd0 = &tree.ifds[0];
        let at = ifd0.get_u64_vec(tags::STRIP_OFFSETS).expect("preview")[0] as usize;
        let child = &ifd0.sub_ifds()[0].ifds[0];
        let raw_at = child.get_u64_vec(tags::STRIP_OFFSETS).expect("raw")[0] as usize;
        assert_eq!(raw_at, at + 2);
        assert_eq!(&out[at..], [1, 1, 2, 2, 2, 2]);
    }

    /// The accessors report what `install` writes.
    #[test]
    fn a_placed_run_reports_its_fields() {
        let strips = [vec![0u8; 5]];
        let placed = place(Variant::Classic, &[BlockRun::strips(&strips)], |_| 7).expect("place");
        assert_eq!(placed[0].offsets(), &Value::Long(vec![7]));
        assert_eq!(placed[0].counts(), &Value::Long(vec![5]));
        let mut ifd = Ifd::new();
        placed[0].install(&mut ifd);
        assert_eq!(ifd.get(tags::STRIP_OFFSETS), Some(placed[0].offsets()));
        assert_eq!(ifd.get(tags::STRIP_BYTE_COUNTS), Some(placed[0].counts()));
    }

    /// A classic file cannot address a block past 4 GiB: placing one there is refused, not
    /// truncated.
    #[test]
    fn a_classic_offset_past_four_gib_is_refused() {
        let strips = [vec![0u8]];
        let got = place(Variant::Classic, &[BlockRun::strips(&strips)], |_| {
            u64::from(u32::MAX) + 1
        });
        assert!(got.is_err(), "{got:?}");
    }

    /// A byte count is the block's length up to the largest `LONG`, and a 4 GiB block is refused,
    /// not truncated. Asserted on the length alone: no test allocates a 4 GiB block.
    #[test]
    fn a_byte_count_past_a_long_is_refused() {
        assert_eq!(byte_count(5).expect("small"), 5);
        assert_eq!(
            byte_count(u32::MAX as usize).expect("largest LONG"),
            u32::MAX
        );
        #[cfg(target_pointer_width = "64")]
        {
            let err = byte_count(u32::MAX as usize + 1).expect_err("4 GiB");
            assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput);
        }
    }
}
