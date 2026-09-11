//! Palette (PLTE) and palette transparency (tRNS) for indexed-colour PNG (PNG spec §11.2.2/§11.3.2).

use gamut_core::{Error, Result};

/// A PNG palette: 1–256 RGB entries, with optional per-entry alpha (written as a tRNS chunk).
///
/// Entries without an alpha value are fully opaque. Indexed images reference entries by index.
#[derive(Debug, Clone)]
pub struct PngPalette {
    rgb: Vec<[u8; 3]>,
    /// Per-entry alpha for the leading entries; entries beyond `alpha.len()` are opaque.
    alpha: Vec<u8>,
}

impl PngPalette {
    /// Builds an opaque palette from 1–256 RGB entries.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if there are zero or more than 256 entries.
    pub fn new(entries: &[[u8; 3]]) -> Result<Self> {
        Self::with_transparency(entries, &[])
    }

    /// Builds a palette with per-entry transparency. `alpha[i]` is the alpha of palette entry `i`;
    /// `alpha` may be shorter than `rgb` (the remaining entries are opaque).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if there are zero or more than 256 RGB entries, or if there
    /// are more alpha values than RGB entries.
    pub fn with_transparency(rgb: &[[u8; 3]], alpha: &[u8]) -> Result<Self> {
        if rgb.is_empty() || rgb.len() > 256 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: palette must have 1..=256 entries",
            ));
        }
        if alpha.len() > rgb.len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: more tRNS entries than palette entries",
            ));
        }
        Ok(Self {
            rgb: rgb.to_vec(),
            alpha: alpha.to_vec(),
        })
    }

    /// Builds a palette from raw `PLTE` and optional `tRNS` chunk payloads (§11.2.2, §11.3.1.1).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the PLTE payload is not a whole number of RGB triples,
    /// holds zero or more than 256 entries, or has fewer entries than the tRNS payload.
    pub(crate) fn from_chunks(plte: &[u8], trns: Option<&[u8]>) -> Result<Self> {
        if !plte.len().is_multiple_of(3) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: PLTE payload must be a whole number of RGB triples",
            ));
        }
        let rgb: Vec<[u8; 3]> = plte
            .as_chunks::<3>()
            .0
            .iter()
            .map(|entry| [entry[0], entry[1], entry[2]])
            .collect();
        Self::with_transparency(&rgb, trns.unwrap_or_default())
    }

    /// The number of palette entries (1–256).
    #[must_use]
    pub fn len(&self) -> usize {
        self.rgb.len()
    }

    /// The RGB triple of entry `index`, or `None` if the index is out of range.
    #[must_use]
    pub fn rgb(&self, index: u8) -> Option<[u8; 3]> {
        self.rgb.get(usize::from(index)).copied()
    }

    /// The alpha of entry `index` (255 for entries beyond the tRNS values), or `None` if the
    /// index is out of range.
    #[must_use]
    pub fn alpha(&self, index: u8) -> Option<u8> {
        if usize::from(index) >= self.rgb.len() {
            return None;
        }
        Some(self.alpha.get(usize::from(index)).copied().unwrap_or(255))
    }

    /// Whether any entry is not fully opaque (i.e. the palette carries transparency).
    #[must_use]
    pub fn has_transparency(&self) -> bool {
        self.alpha.iter().any(|&alpha| alpha != 255)
    }

    /// Always `false` — a palette has at least one entry (kept for API completeness).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rgb.is_empty()
    }

    /// The palette reduced to the entries `used` marks, with duplicates merged and the `tRNS`
    /// trailing-opaque bytes trimmed, plus the old-index → new-index map that rewrites an image's
    /// indices onto it.
    ///
    /// Three redundancies a caller-supplied palette may carry that one built from the pixels
    /// cannot, and what each costs the file:
    ///
    /// - an entry `used` does not mark: 3 `PLTE` bytes naming a colour nothing in the file reads;
    /// - a later entry with the same RGB **and** the same alpha as an earlier one: the same 3
    ///   bytes, for a colour the earlier entry already names;
    /// - a trailing opaque `tRNS` entry: 1 byte the chunk may simply not carry (§11.3.2.1).
    ///
    /// The saving is rarely those bytes. It is that `PLTE` is incompressible and that a shorter
    /// palette may fit a smaller index bit depth — a 256-entry palette holding three colours drops
    /// 759 `PLTE` bytes *and* takes the index stream from 8 bits per pixel to 2.
    ///
    /// Nothing is lost: every surviving entry keeps its RGB and its alpha byte for byte, and the
    /// map sends each marked old index to the entry that holds the colour it named, so the pixels
    /// a decoder resolves are the ones the caller supplied. Surviving entries keep the caller's
    /// relative order — reordering them is a separate, heuristic question (#612).
    ///
    /// `used[i]` beyond [`Self::len`] is ignored. `remap[i]` for an unmarked or out-of-range `i`
    /// is 0, which no index reaching a remapped image can be, because the caller marks every index
    /// its image uses.
    ///
    /// The result is a valid palette without a length check: at most 256 entries go in so at most
    /// 256 come out, and the one caller ([`crate::PngEncoder::encode_indexed8`]) marks the index
    /// of every pixel in an image that [`gamut_core::ImageRef`] has already refused to build empty,
    /// so at least one entry is always marked.
    pub(crate) fn cleaned(&self, used: &[bool; 256]) -> (Self, [u8; 256]) {
        let mut kept: Vec<([u8; 3], u8)> = Vec::new();
        let mut remap = [0u8; 256];
        for (index, &rgb) in self.rgb.iter().enumerate() {
            if !used[index] {
                continue;
            }
            let entry = (rgb, self.alpha.get(index).copied().unwrap_or(OPAQUE));
            let position = kept.iter().position(|&k| k == entry).unwrap_or_else(|| {
                kept.push(entry);
                kept.len() - 1
            });
            remap[index] = position as u8;
        }
        let mut alpha: Vec<u8> = kept.iter().map(|&(_, a)| a).collect();
        trim_trailing_opaque(&mut alpha);
        let cleaned = Self {
            rgb: kept.into_iter().map(|(rgb, _)| rgb).collect(),
            alpha,
        };
        (cleaned, remap)
    }

    /// The PLTE chunk payload: RGB triples, flattened.
    pub(crate) fn plte(&self) -> Vec<u8> {
        self.rgb.iter().flatten().copied().collect()
    }

    /// The tRNS chunk payload (the alpha values), or `None` if the palette is fully opaque.
    pub(crate) fn trns(&self) -> Option<&[u8]> {
        if self.alpha.is_empty() {
            None
        } else {
            Some(&self.alpha)
        }
    }
}

/// The alpha a palette entry has when `tRNS` does not carry one for it (§11.3.2.1).
pub(crate) const OPAQUE: u8 = 255;

/// Drops the trailing fully-opaque entries a `tRNS` chunk is allowed to omit: a decoder reads
/// every entry past the chunk's end as opaque (§11.3.2.1), so those bytes say nothing the absence
/// of the bytes does not already say.
///
/// One owner for the rule, because both palette paths need it and a rule restated twice is a rule
/// that can drift: the encoder-derived palette trims the alphas it collects
/// ([`crate::reduce`]), and a caller-supplied one trims [`PngPalette::cleaned`]'s.
///
/// Written as a search for the last entry that must stay rather than as a pop-until loop. The two
/// compute the same length, but the loop's condition has a form -- `last() == Some(&OPAQUE)` -- in
/// which inverting the comparison never terminates, because an emptied vector answers `None` and
/// `None != Some(&OPAQUE)` holds forever. That is a hang no test can distinguish from a slow one,
/// so the shape that cannot express it is the one to write.
pub(crate) fn trim_trailing_opaque(alphas: &mut Vec<u8>) {
    let keep = alphas
        .iter()
        .rposition(|&a| a != OPAQUE)
        .map_or(0, |i| i + 1);
    alphas.truncate(keep);
}

#[cfg(test)]
mod tests {

    /// A constructed palette is never empty, and `is_empty` says so.
    ///
    /// The method had no caller in any test, so it could return a constant `true` unnoticed
    /// (#110). Its counterpart -- a constant `false` -- is a genuinely equivalent mutant rather
    /// than a gap: `with_transparency` is the only constructor and it refuses an empty `rgb`, so
    /// `self.rgb.is_empty()` is invariantly false for every value that exists. The refusal is
    /// asserted here too, because that invariant is the whole reason the equivalence holds.
    #[test]
    fn a_constructed_palette_is_never_empty() {
        let one = PngPalette::new(&[[1, 2, 3]]).expect("one entry is a palette");
        assert!(!one.is_empty());
        assert_eq!(one.len(), 1);

        assert!(
            PngPalette::new(&[]).is_err(),
            "the constructor is what makes is_empty invariantly false"
        );
    }
    use super::*;

    #[test]
    fn rejects_invalid_sizes() {
        assert!(PngPalette::new(&[]).is_err());
        assert!(PngPalette::new(&vec![[0, 0, 0]; 257]).is_err());
        assert!(PngPalette::with_transparency(&[[0, 0, 0]], &[1, 2]).is_err());
        assert!(PngPalette::new(&[[1, 2, 3]]).is_ok());
    }

    #[test]
    fn serialises_plte_and_trns() {
        let p = PngPalette::with_transparency(&[[1, 2, 3], [4, 5, 6]], &[0]).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p.plte(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(p.trns(), Some(&[0u8][..]));
        let opaque = PngPalette::new(&[[7, 8, 9]]).unwrap();
        assert_eq!(opaque.trns(), None);
    }

    #[test]
    fn from_chunks_round_trips_serialisation() {
        let original =
            PngPalette::with_transparency(&[[1, 2, 3], [4, 5, 6], [7, 8, 9]], &[0, 128]).unwrap();
        let parsed = PngPalette::from_chunks(&original.plte(), original.trns()).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.rgb(0), Some([1, 2, 3]));
        assert_eq!(parsed.rgb(2), Some([7, 8, 9]));
        assert_eq!(parsed.rgb(3), None);
        assert_eq!(parsed.alpha(0), Some(0));
        assert_eq!(parsed.alpha(1), Some(128));
        assert_eq!(parsed.alpha(2), Some(255)); // beyond tRNS: opaque
        assert_eq!(parsed.alpha(3), None);
        assert!(parsed.has_transparency());
        assert!(!PngPalette::new(&[[0, 0, 0]]).unwrap().has_transparency());
    }

    /// Marks `indices` (and nothing else) as used, the way `encode_indexed8` does.
    fn used_by(indices: &[u8]) -> [bool; 256] {
        let mut used = [false; 256];
        for &index in indices {
            used[usize::from(index)] = true;
        }
        used
    }

    /// [`trim_trailing_opaque`] removes exactly the run of opaque entries at the end.
    ///
    /// It stops at the last non-opaque entry rather than removing every opaque one, because a
    /// `tRNS` chunk is positional: entry 1 below is opaque and has to stay, or entry 2's alpha
    /// would land on entry 1.
    #[test]
    fn trailing_opaque_alphas_are_the_ones_trns_may_omit() {
        let mut alphas = vec![0, OPAQUE, 128, OPAQUE, OPAQUE];
        trim_trailing_opaque(&mut alphas);
        assert_eq!(alphas, vec![0, OPAQUE, 128]);

        // A wholly opaque run leaves nothing, which is the chunk not being written at all.
        let mut all_opaque = vec![OPAQUE; 4];
        trim_trailing_opaque(&mut all_opaque);
        assert!(all_opaque.is_empty());

        // Nothing to trim is not an error.
        let mut none = vec![7, 8];
        trim_trailing_opaque(&mut none);
        assert_eq!(none, vec![7, 8]);
    }

    /// [`PngPalette::cleaned`] drops an entry no index marks, and renumbers the survivors.
    #[test]
    fn cleaning_drops_an_entry_no_index_marks() {
        let palette = PngPalette::new(&[[0, 0, 0], [1, 1, 1], [2, 2, 2], [3, 3, 3]]).unwrap();
        let (cleaned, remap) = palette.cleaned(&used_by(&[1, 3]));

        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned.rgb(0), Some([1, 1, 1]));
        assert_eq!(cleaned.rgb(1), Some([3, 3, 3]));
        // The survivors keep the caller's relative order, so 1 lands before 3.
        assert_eq!(remap[1], 0);
        assert_eq!(remap[3], 1);
    }

    /// [`PngPalette::cleaned`] merges two entries naming the same colour, sending both old indices
    /// to the surviving one.
    #[test]
    fn cleaning_merges_entries_that_name_the_same_colour() {
        let palette = PngPalette::new(&[[9, 9, 9], [4, 4, 4], [9, 9, 9]]).unwrap();
        let (cleaned, remap) = palette.cleaned(&used_by(&[0, 1, 2]));

        assert_eq!(cleaned.len(), 2, "the repeated colour is written once");
        assert_eq!(cleaned.rgb(0), Some([9, 9, 9]));
        assert_eq!(cleaned.rgb(1), Some([4, 4, 4]));
        assert_eq!(
            remap[2], remap[0],
            "the duplicate resolves to the first entry"
        );
        assert_eq!(remap[1], 1);
    }

    /// Two entries with the same RGB but different alphas are different colours, so
    /// [`PngPalette::cleaned`] keeps both.
    ///
    /// Merging them would silently repaint every pixel using one of them.
    #[test]
    fn an_entry_is_its_alpha_as_well_as_its_rgb() {
        let palette = PngPalette::with_transparency(&[[9, 9, 9], [9, 9, 9]], &[0, 200]).unwrap();
        let (cleaned, remap) = palette.cleaned(&used_by(&[0, 1]));

        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned.alpha(0), Some(0));
        assert_eq!(cleaned.alpha(1), Some(200));
        assert_ne!(remap[0], remap[1]);
    }

    /// An entry the palette leaves out of `tRNS` is opaque (§11.3.2.1), so
    /// [`PngPalette::cleaned`] must compare it as opaque rather than as absent.
    ///
    /// Entry 1 below carries an explicit `OPAQUE` and entry 2 carries none; they are the same
    /// colour and must merge, which they cannot if a missing alpha is treated as its own value.
    #[test]
    fn a_missing_trns_entry_is_opaque_when_entries_are_compared() {
        let palette =
            PngPalette::with_transparency(&[[0, 0, 0], [5, 5, 5], [5, 5, 5]], &[0, OPAQUE])
                .unwrap();
        let (cleaned, remap) = palette.cleaned(&used_by(&[0, 1, 2]));

        assert_eq!(cleaned.len(), 2);
        assert_eq!(remap[2], remap[1]);
    }

    /// [`PngPalette::cleaned`] hands back a palette whose `tRNS` payload is already trimmed, so
    /// the encoder writes the shortest chunk §11.3.2.1 allows.
    #[test]
    fn a_cleaned_palette_carries_a_trimmed_trns() {
        let palette =
            PngPalette::with_transparency(&[[0, 0, 0], [1, 1, 1], [2, 2, 2]], &[0, OPAQUE, OPAQUE])
                .unwrap();
        let (cleaned, _) = palette.cleaned(&used_by(&[0, 1, 2]));
        assert_eq!(cleaned.trns(), Some(&[0u8][..]));

        // Nothing transparent survives: no chunk at all.
        let (opaque, _) = palette.cleaned(&used_by(&[1, 2]));
        assert_eq!(opaque.trns(), None);
    }

    #[test]
    fn from_chunks_rejects_malformed_payloads() {
        assert!(PngPalette::from_chunks(&[1, 2, 3, 4], None).is_err()); // not a triple multiple
        assert!(PngPalette::from_chunks(&[], None).is_err()); // zero entries
        assert!(PngPalette::from_chunks(&[0; 771], None).is_err()); // 257 entries
        assert!(PngPalette::from_chunks(&[1, 2, 3], Some(&[0, 0])).is_err()); // tRNS too long
    }
}
