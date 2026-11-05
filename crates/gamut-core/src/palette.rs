//! [`Palette`]: the colour table an indexed ([`crate::Indexed8`]) image resolves its indices
//! through.

use crate::{Error, Result};

/// The alpha of an opaque entry.
const OPAQUE: u8 = u8::MAX;

/// A colour table of 1–256 8-bit RGB entries, with optional per-entry alpha.
///
/// One representation for every format that carries a palette (PNG `PLTE`/`tRNS`, TIFF
/// `ColorMap`, …). Entries beyond the alpha values supplied are fully opaque, so a palette with no
/// alpha is an opaque one. An [`crate::Indexed8`] sample is an index into it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Palette {
    rgb: Vec<[u8; 3]>,
    /// Per-entry alpha for the leading entries; entries beyond `alpha.len()` are opaque.
    alpha: Vec<u8>,
}

impl Palette {
    /// Builds an opaque palette from 1–256 RGB entries.
    ///
    /// # Errors
    ///
    /// Returns an [`crate::ErrorKind::InvalidInput`] error if there are zero or more than 256
    /// entries.
    pub fn new(rgb: &[[u8; 3]]) -> Result<Self> {
        Self::with_alpha(rgb, &[])
    }

    /// Builds a palette with per-entry alpha: `alpha[i]` is the alpha of entry `i`, and `alpha`
    /// may be shorter than `rgb` (the remaining entries are opaque).
    ///
    /// # Errors
    ///
    /// Returns an [`crate::ErrorKind::InvalidInput`] error if there are zero or more than 256 RGB
    /// entries, or more alpha values than RGB entries.
    pub fn with_alpha(rgb: &[[u8; 3]], alpha: &[u8]) -> Result<Self> {
        if !(1..=256).contains(&rgb.len()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "palette must have 1..=256 entries",
            ));
        }
        if alpha.len() > rgb.len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "palette has more alpha values than entries",
            ));
        }
        // Trailing opaque values are dropped, so the stored alpha is the shortest one that resolves
        // every entry: equal observable palettes then compare and hash equal however they were built.
        let significant = alpha
            .iter()
            .rposition(|&a| a != OPAQUE)
            .map_or(0, |i| i + 1);
        Ok(Self {
            rgb: rgb.to_vec(),
            alpha: alpha[..significant].to_vec(),
        })
    }

    /// The 1–256 RGB entries, in index order.
    #[must_use]
    pub fn rgb_entries(&self) -> &[[u8; 3]] {
        &self.rgb
    }

    /// The RGB triple of entry `index`, or `None` if the index is out of range.
    #[must_use]
    pub fn rgb(&self, index: u8) -> Option<[u8; 3]> {
        self.rgb.get(usize::from(index)).copied()
    }

    /// The alpha of entry `index` (255 for entries beyond the supplied alpha values), or `None` if
    /// the index is out of range.
    #[must_use]
    pub fn alpha(&self, index: u8) -> Option<u8> {
        let index = usize::from(index);
        (index < self.rgb.len()).then(|| self.alpha.get(index).copied().unwrap_or(OPAQUE))
    }

    /// Whether any entry is not fully opaque.
    #[must_use]
    pub fn has_transparency(&self) -> bool {
        // `with_alpha` drops trailing opaque values, so any stored value ends in a transparent one.
        !self.alpha.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn new_accepts_one_through_256_entries() {
        assert_eq!(
            Palette::new(&[[1, 2, 3]]).unwrap().rgb_entries(),
            &[[1, 2, 3]]
        );
        assert_eq!(
            Palette::new(&[[0; 3]; 256]).unwrap().rgb_entries().len(),
            256
        );
        for bad in [&[][..], &[[0u8; 3]; 257][..]] {
            let err = Palette::new(bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(
                err.static_message(),
                Some("palette must have 1..=256 entries")
            );
        }
    }

    #[test]
    fn with_alpha_rejects_more_alpha_than_entries() {
        assert!(Palette::with_alpha(&[[0; 3]; 2], &[0, 0]).is_ok());
        let err = Palette::with_alpha(&[[0; 3]; 2], &[0, 0, 0]).unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("palette has more alpha values than entries")
        );
    }

    #[test]
    fn entries_resolve_rgb_and_alpha_by_index() {
        let palette = Palette::with_alpha(&[[1, 2, 3], [4, 5, 6], [7, 8, 9]], &[10]).unwrap();
        assert_eq!(palette.rgb_entries(), &[[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
        assert_eq!(palette.rgb(1), Some([4, 5, 6]));
        assert_eq!(palette.rgb(3), None);
        assert_eq!(palette.alpha(0), Some(10));
        assert_eq!(palette.alpha(2), Some(255));
        assert_eq!(palette.alpha(3), None);
    }

    #[test]
    fn with_alpha_drops_trailing_opaque_values() {
        let rgb = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
        let opaque = Palette::new(&rgb).unwrap();
        assert_eq!(Palette::with_alpha(&rgb, &[255]).unwrap(), opaque);
        assert_eq!(Palette::with_alpha(&rgb, &[255, 255, 255]).unwrap(), opaque);
        // An opaque value before a transparent one is significant and kept.
        let inner = Palette::with_alpha(&rgb, &[255, 0, 255]).unwrap();
        assert_eq!(inner, Palette::with_alpha(&rgb, &[255, 0]).unwrap());
        assert_eq!(inner.alpha, [255, 0]);
    }

    #[test]
    fn transparency_is_any_entry_below_opaque() {
        assert!(!Palette::new(&[[0; 3]; 2]).unwrap().has_transparency());
        assert!(
            !Palette::with_alpha(&[[0; 3]; 2], &[255, 255])
                .unwrap()
                .has_transparency()
        );
        assert!(
            Palette::with_alpha(&[[0; 3]; 2], &[255, 254])
                .unwrap()
                .has_transparency()
        );
    }
}
