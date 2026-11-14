//! The IFD walk: every directory of the file — the main chain and, recursively, its `SubIFDs`
//! children — in encounter order with its position in the IFD tree, and the choice of the raw
//! image IFD among them.

use gamut_core::{Error, Result};
use gamut_ifd::{ByteOrder, Ifd, IfdReader, TiffFile, Variant, read_ifd_at};

use crate::tags;
use crate::values::{PhotometricInterpretation, dng_photometric};

/// Whether `ifd` holds a raw image (`PhotometricInterpretation` is CFA or LinearRaw).
fn is_raw_ifd(ifd: &Ifd) -> bool {
    matches!(
        ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|c| u16::try_from(c).ok())
            .and_then(dng_photometric),
        Some(PhotometricInterpretation::Cfa | PhotometricInterpretation::LinearRaw)
    )
}

/// An upper bound on the `SubIFDs` nesting depth the decoder follows, bounding hostile pointer
/// graphs; legitimate DNG trees (IFD 0 → raw / enhanced / mask sub-IFDs) are two levels.
const MAX_SUBIFD_DEPTH: usize = 8;

/// Where a walked directory sits in the IFD tree: the main-chain page it is or descends from,
/// then its position in each successive `SubIFDs` array below that page — empty for the page
/// itself. The public form is [`SubIfdExtra`](crate::SubIfdExtra)'s `page` and `path`.
type TreePosition = (usize, Vec<usize>);

/// The state one [`walk_ifds`] threads through its recursion: the file being read and every
/// directory collected so far, with its [`TreePosition`] at the same index.
struct IfdWalk<'a> {
    data: &'a [u8],
    order: ByteOrder,
    variant: Variant,
    visited: Vec<u64>,
    ifds: Vec<Ifd>,
    positions: Vec<TreePosition>,
}

/// Collects every IFD in the file — the top-level chain plus, recursively, every `SubIFDs`
/// child — in encounter order, plus each one's [`TreePosition`] at the same index. The
/// main-chain directories are the ones with an empty path; the last of them is where C2PA 2.4
/// §A.3.6 places the manifest store, the ones before it are the pages
/// [`DecodedDng::interior_extra`](crate::DecodedDng::interior_extra) covers, and the rest are what
/// [`DecodedDng::sub_ifd_extra`](crate::DecodedDng::sub_ifd_extra) covers. Lenient by design: an
/// unreadable child is skipped rather than failing the whole decode, while offset de-duplication
/// and the depth cap terminate hostile pointer cycles.
///
/// Every directory is walked once, from where the file first places it: the main-chain page
/// offsets seed the visited set, so a `SubIFDs` entry naming a page — IFD 0 itself, or any later
/// page — is skipped like any other already-visited offset rather than re-walked as a child.
/// Re-walking it would report the page's fields in
/// [`DecodedDng::sub_ifd_extra`](crate::DecodedDng::sub_ifd_extra) as well as in its page channel,
/// and a self-referencing entry would duplicate the page in the tree. This seed is the decoder's
/// alone: `deconstruct`'s audit walk does not share it, so it follows such an entry as an ordinary
/// child and draws no skipped-entry or cycle finding for the entry itself.
pub(super) fn walk_ifds(file: &TiffFile, data: &[u8]) -> (Vec<Ifd>, Vec<TreePosition>) {
    // `file` came from `read` over these same bytes, so the chain re-walk below meets the same
    // pages without error; its raw directories are only what carries each page's offset, which
    // `Ifd` does not keep.
    let visited = IfdReader::open(data)
        .map(|mut reader| {
            reader
                .ifds()
                .map_while(Result::ok)
                .map(|raw| raw.offset)
                .collect()
        })
        .unwrap_or_default();
    let mut walk = IfdWalk {
        data,
        order: file.order,
        variant: file.variant,
        visited,
        ifds: Vec::new(),
        positions: Vec::new(),
    };
    for (page, ifd) in file.ifds.iter().enumerate() {
        collect_sub_ifds(ifd, (page, Vec::new()), &mut walk);
    }
    (walk.ifds, walk.positions)
}

/// Pushes `ifd` at `position` and recurses into its `SubIFDs` children (see [`walk_ifds`]).
///
/// The nesting depth is the path's length plus one (a page is depth 1), so the cap reads the
/// position rather than a counter threaded beside it.
fn collect_sub_ifds(ifd: &Ifd, position: TreePosition, walk: &mut IfdWalk) {
    walk.ifds.push(ifd.clone());
    walk.positions.push(position.clone());
    if position.1.len() + 1 >= MAX_SUBIFD_DEPTH {
        return;
    }
    // Offsets are read at full width: a BigTIFF writes `SubIFDs` as `LONG8`, whose values only
    // matter past 4 GiB — exactly where a u32 reading would fail.
    let Some(offsets) = ifd.get_u64_vec(tags::SUB_IFDS) else {
        return;
    };
    // Every array entry is numbered, followed or not, so a path indexes the array as stored.
    for (index, offset) in offsets.into_iter().enumerate() {
        if walk.visited.contains(&offset) {
            continue;
        }
        walk.visited.push(offset);
        if let Ok(sub) = read_ifd_at(walk.data, offset, walk.order, walk.variant) {
            let mut child = position.clone();
            child.1.push(index);
            collect_sub_ifds(&sub, child, walk);
        }
    }
}

/// Selects the full-resolution raw IFD from the walked forest (real DNGs keep the raw in a
/// sub-IFD of IFD 0, but TIFF/EP permits it anywhere).
///
/// Prefers an IFD whose `NewSubFileType` is 0 (the main image; the tag defaults to 0 when
/// absent) with a raw photometry, falling back to any raw-photometry IFD.
pub(super) fn select_raw_ifd(ifds: &[Ifd]) -> Result<usize> {
    let mut fallback = None;
    for (index, ifd) in ifds.iter().enumerate() {
        if !is_raw_ifd(ifd) {
            continue;
        }
        if ifd.get_u32(tags::NEW_SUBFILE_TYPE).unwrap_or(0) == 0 {
            return Ok(index);
        }
        if fallback.is_none() {
            fallback = Some(index);
        }
    }
    fallback
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: no raw image IFD found"))
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{Value, read};

    use super::*;
    use crate::decoder::tests::sub_ifds_naming_main_chain_pages;

    #[test]
    fn is_raw_ifd_only_for_raw_photometry() {
        let mut ifd = Ifd::new();
        // An RGB preview IFD (PhotometricInterpretation = 2) is not a raw image.
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
        assert!(!is_raw_ifd(&ifd));
        // A CFA IFD (32803) is.
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        assert!(is_raw_ifd(&ifd));
        // A missing tag is not raw either.
        assert!(!is_raw_ifd(&Ifd::new()));
    }

    /// The `SubIFDs` walk is depth-capped: a 10-deep nested chain yields exactly
    /// `MAX_SUBIFD_DEPTH` IFDs (the cap stops recursion, not collection of the capped node).
    #[test]
    fn walk_ifds_caps_hostile_nesting_depth() {
        // Innermost first: each level wraps the previous as its SubIFDs child.
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        for _ in 0..9 {
            let mut parent = Ifd::new();
            parent.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
            parent.set_sub_ifd(tags::SUB_IFDS, vec![ifd]);
            ifd = parent;
        }
        let bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        let file = read(&bytes).expect("read");
        assert_eq!(walk_ifds(&file, &bytes).0.len(), MAX_SUBIFD_DEPTH);
    }

    /// Every walked directory carries its tree position: the page it is or descends from, and
    /// its `SubIFDs` array position at each level below — empty for the pages themselves.
    /// Sub-IFDs of an earlier page are pushed before a later page and the later page's own
    /// sub-IFDs after it, so a page's index is never the last directory collected, and a
    /// grandchild's path extends its parent's rather than restarting.
    #[test]
    fn walk_ifds_positions_every_directory_in_the_tree() {
        let width = |w: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![w]));
            ifd
        };
        let mut page0 = width(1);
        page0.set_sub_ifd(tags::SUB_IFDS, vec![width(2)]);
        let mut nested = width(4);
        nested.set_sub_ifd(tags::SUB_IFDS, vec![width(5)]);
        let mut page1 = width(3);
        page1.set_sub_ifd(tags::SUB_IFDS, vec![width(6), nested]);
        let bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![page0, page1],
        })
        .expect("write");
        let file = read(&bytes).expect("read");
        let (ifds, positions) = walk_ifds(&file, &bytes);
        let widths: Vec<_> = ifds.iter().map(|i| i.get_u32(tags::IMAGE_WIDTH)).collect();
        assert_eq!(widths, [1, 2, 3, 6, 4, 5].map(Some));
        assert_eq!(
            positions,
            [
                (0, vec![]),
                (0, vec![0]),
                (1, vec![]),
                (1, vec![0]),
                (1, vec![1]),
                (1, vec![1, 0]),
            ]
        );
    }

    /// A `SubIFDs` entry the walk does not follow — here a second pointer to an already-visited
    /// directory — still takes its array position, so the entry after it keeps the index the
    /// file stores it at rather than shifting down into the skipped slot.
    #[test]
    fn walk_ifds_numbers_skipped_sub_ifd_entries() {
        let width = |w: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![w]));
            ifd
        };
        let mut page = width(1);
        page.set_sub_ifd(tags::SUB_IFDS, vec![width(2), width(3), width(4)]);
        let mut bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![page],
        })
        .expect("write");
        // Point the second entry at the first child, so the walk skips it as visited.
        let file = read(&bytes).expect("read");
        let offsets = file.ifds[0].get_u32_vec(tags::SUB_IFDS).expect("offsets");
        let array: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        let at = bytes
            .windows(array.len())
            .position(|w| w == array)
            .expect("the SubIFDs array is stored out of line")
            + 4;
        bytes[at..at + 4].copy_from_slice(&offsets[0].to_le_bytes());
        let file = read(&bytes).expect("read");
        let (ifds, positions) = walk_ifds(&file, &bytes);
        let widths: Vec<_> = ifds.iter().map(|i| i.get_u32(tags::IMAGE_WIDTH)).collect();
        assert_eq!(widths, [1, 2, 4].map(Some));
        assert_eq!(positions, [(0, vec![]), (0, vec![0]), (0, vec![2])]);
    }

    /// A `SubIFDs` entry naming a main-chain page — IFD 0 itself, or a later page — is skipped as
    /// already visited, so each page is walked once, as a page: the tree holds no duplicate of
    /// IFD 0 and no child copy of page 1, while the skipped entries keep their array positions.
    #[test]
    fn walk_ifds_skips_sub_ifd_entries_naming_a_main_chain_page() {
        let bytes = sub_ifds_naming_main_chain_pages();
        let file = read(&bytes).expect("read");
        let (ifds, positions) = walk_ifds(&file, &bytes);
        let names: Vec<_> = ifds.iter().map(|i| i.get(305).cloned()).collect();
        assert_eq!(
            names,
            ["ifd0", "raw", "page"].map(|s| Some(Value::Ascii(s.to_owned())))
        );
        assert_eq!(positions, [(0, vec![]), (0, vec![0]), (1, vec![])]);
    }

    /// With several raw-photometry IFDs, the main image (`NewSubFileType` 0) wins even when a
    /// reduced-resolution raw appears first.
    #[test]
    fn select_raw_ifd_prefers_the_main_image() {
        let raw_ifd = |nsft: u32, width: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![nsft]));
            ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![width]));
            ifd
        };
        // A reduced-resolution raw (type 1) first, the main raw (type 0) second.
        let ifds = vec![raw_ifd(1, 2), raw_ifd(0, 4)];
        assert_eq!(select_raw_ifd(&ifds).unwrap(), 1);
        // With no type-0 raw at all, the first raw IFD is the fallback.
        let ifds = vec![raw_ifd(1, 2), raw_ifd(1, 4)];
        assert_eq!(select_raw_ifd(&ifds).unwrap(), 0);
        // No raw photometry anywhere is an error.
        assert!(select_raw_ifd(&[Ifd::new()]).is_err());
    }
}
