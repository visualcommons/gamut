//! The 256-entry RGB colour table for palette (indexed-colour) TIFF images.

use gamut_core::{Error, Palette, Result};

/// The refusal for a palette carrying per-entry alpha, which a TIFF `ColorMap` has no field for.
pub(crate) const PALETTE_ALPHA_REFUSAL: &str =
    "TIFF: a ColorMap holds no alpha, so a palette with transparent entries cannot be written";

/// A 256-entry 8-bit RGB colour table — the palette a
/// [`PhotometricInterpretation::Palette`](crate::ifd::PhotometricInterpretation::Palette) image's
/// indices select into.
///
/// On disk TIFF stores this as the `ColorMap` tag: 3×256 16-bit values laid out as all reds, then
/// all greens, then all blues, with each 8-bit channel scaled to 16-bit by ×257 (and read back by
/// taking the high byte). `Palette8` holds the natural 8-bit `[[u8; 3]; 256]` form and owns both
/// conversions, so no other code indexes a flat colour-map vector by hand — the previous decoder
/// kept the map as a bare `Vec<u32>` and recovered channels with `cm[i] >> 8`, `cm[256 + i] >> 8`,
/// `cm[512 + i] >> 8`, which this type replaces with [`Palette8::entry`].
///
/// Crate-private: the public palette is the workspace's [`gamut_core::Palette`], which
/// [`from_palette`](Self::from_palette) lays out as the fixed-size table TIFF stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Palette8 {
    entries: [[u8; 3]; 256],
}

impl Palette8 {
    /// The `ColorMap` table for `palette`: its entries in order, and black past its last one.
    ///
    /// TIFF 6.0 §5 fixes the `ColorMap` at `2**BitsPerSample` entries — 256 for the 8-bit indices
    /// this crate writes — while a [`Palette`] may hold 1–256; the entries it lacks are never
    /// selected, because [`TiffEncoder::encode_indexed8`](crate::TiffEncoder::encode_indexed8)
    /// refuses an index past the palette before it gets here.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error
    /// ([`PALETTE_ALPHA_REFUSAL`]) if the palette has a transparent entry: a `ColorMap` is colour
    /// only, so the alpha would be dropped without a word.
    pub(crate) fn from_palette(palette: &Palette) -> Result<Self> {
        if palette.has_transparency() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                PALETTE_ALPHA_REFUSAL,
            ));
        }
        let mut entries = [[0u8; 3]; 256];
        for (entry, &rgb) in entries.iter_mut().zip(palette.rgb_entries()) {
            *entry = rgb;
        }
        Ok(Self { entries })
    }

    /// Builds a palette from a TIFF `ColorMap`: 3×256 16-bit values (reds, then greens, then blues),
    /// each reduced to 8 bits by taking its high byte. Accepts the `u32`-widened values the IFD
    /// reader yields for a `SHORT` field.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `colormap.len() != 3 * 256`.
    pub(crate) fn from_tiff_colormap(colormap: &[u32]) -> Result<Self> {
        if colormap.len() != 3 * 256 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: ColorMap must have 3*256 entries",
            ));
        }
        let mut entries = [[0u8; 3]; 256];
        for (i, e) in entries.iter_mut().enumerate() {
            *e = [
                (colormap[i] >> 8) as u8,
                (colormap[256 + i] >> 8) as u8,
                (colormap[512 + i] >> 8) as u8,
            ];
        }
        Ok(Self { entries })
    }

    /// The TIFF `ColorMap` encoding of this palette: 3×256 16-bit values (reds, then greens, then
    /// blues), each 8-bit channel scaled to 16-bit by ×257.
    #[must_use]
    pub(crate) fn to_tiff_colormap(&self) -> Vec<u16> {
        let mut cm = vec![0u16; 3 * 256];
        for (i, e) in self.entries.iter().enumerate() {
            cm[i] = u16::from(e[0]) * 257;
            cm[256 + i] = u16::from(e[1]) * 257;
            cm[512 + i] = u16::from(e[2]) * 257;
        }
        cm
    }

    /// The RGB triple for palette index `idx`.
    #[must_use]
    pub(crate) fn entry(&self, idx: u8) -> [u8; 3] {
        self.entries[idx as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full 256-entry palette whose entry i is (i, 255-i, i^0x55), so each channel differs —
    /// catches a transposed plane.
    fn ramp() -> Palette8 {
        let rgb: Vec<[u8; 3]> = (0..=255u8).map(|i| [i, 255 - i, i ^ 0x55]).collect();
        Palette8::from_palette(&Palette::new(&rgb).unwrap()).unwrap()
    }

    #[test]
    fn from_palette_keeps_every_entry_in_order() {
        let p = ramp();
        assert_eq!(p.entry(0), [0, 255, 0x55]);
        assert_eq!(p.entry(1), [1, 254, 1 ^ 0x55]);
        assert_eq!(p.entry(255), [255, 0, 255 ^ 0x55]);
    }

    /// A short palette fills its own entries and leaves the rest of the fixed table black.
    #[test]
    fn from_palette_pads_a_short_palette_with_black() {
        let p = Palette8::from_palette(&Palette::new(&[[1, 2, 3], [4, 5, 6]]).unwrap()).unwrap();
        assert_eq!(p.entry(0), [1, 2, 3]);
        assert_eq!(p.entry(1), [4, 5, 6]);
        assert_eq!(p.entry(2), [0, 0, 0]);
        assert_eq!(p.entry(255), [0, 0, 0]);
    }

    /// A transparent entry has no `ColorMap` field to go in, so the palette is refused; an alpha
    /// list that is all opaque is not transparency and is accepted.
    #[test]
    fn from_palette_refuses_a_palette_with_transparency() {
        let rgb = [[1, 2, 3], [4, 5, 6]];
        let err =
            Palette8::from_palette(&Palette::with_alpha(&rgb, &[255, 7]).unwrap()).unwrap_err();
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(err.static_message(), Some(PALETTE_ALPHA_REFUSAL));
        assert!(Palette8::from_palette(&Palette::with_alpha(&rgb, &[255, 255]).unwrap()).is_ok());
    }

    #[test]
    fn from_tiff_colormap_validates_length() {
        assert!(Palette8::from_tiff_colormap(&[0u32; 767]).is_err());
        assert!(Palette8::from_tiff_colormap(&[0u32; 768]).is_ok());
    }

    #[test]
    fn colormap_roundtrip_is_lossless() {
        let p = ramp();
        // 8 -> 16 (×257) -> 8 (high byte) is the identity, so the palette survives a round-trip.
        let cm: Vec<u32> = p.to_tiff_colormap().iter().map(|&v| u32::from(v)).collect();
        let back = Palette8::from_tiff_colormap(&cm).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn colormap_layout_is_planar_rgb() {
        let p = ramp();
        let cm = p.to_tiff_colormap();
        // Reds occupy [0,256), greens [256,512), blues [512,768); ×257 scales 8-bit to 16-bit.
        // Entry 1: red=1, green=254, blue=(1^0x55), each ×257.
        assert_eq!(cm[1], 257);
        assert_eq!(cm[256 + 1], 254 * 257);
        assert_eq!(cm[512 + 1], u16::from(1u8 ^ 0x55) * 257);
    }
}
