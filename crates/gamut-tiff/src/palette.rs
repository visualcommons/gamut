//! The 256-entry RGB colour table for palette (indexed-colour) TIFF images.

use gamut_core::{Error, Result};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette8 {
    entries: [[u8; 3]; 256],
}

impl Palette8 {
    /// Builds a palette from 256 interleaved 8-bit RGB triples (`256 * 3` bytes; entry `i` is
    /// `rgb[3 * i..3 * i + 3]`).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `rgb.len() != 256 * 3`.
    pub fn from_rgb_triples(rgb: &[u8]) -> Result<Self> {
        if rgb.len() != 256 * 3 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: palette must be 256 RGB entries",
            ));
        }
        let mut entries = [[0u8; 3]; 256];
        for (i, e) in entries.iter_mut().enumerate() {
            *e = [rgb[3 * i], rgb[3 * i + 1], rgb[3 * i + 2]];
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
    pub fn from_tiff_colormap(colormap: &[u32]) -> Result<Self> {
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
    pub fn to_tiff_colormap(&self) -> Vec<u16> {
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
    pub fn entry(&self, idx: u8) -> [u8; 3] {
        self.entries[idx as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_triples() -> Vec<u8> {
        // Entry i = (i, 255-i, i^0x55), so each channel differs — catches a transposed plane.
        let mut rgb = vec![0u8; 256 * 3];
        for i in 0..256 {
            rgb[3 * i] = i as u8;
            rgb[3 * i + 1] = (255 - i) as u8;
            rgb[3 * i + 2] = (i as u8) ^ 0x55;
        }
        rgb
    }

    #[test]
    fn from_rgb_triples_validates_length() {
        assert!(Palette8::from_rgb_triples(&[0u8; 767]).is_err());
        assert!(Palette8::from_rgb_triples(&[0u8; 769]).is_err());
        let p = Palette8::from_rgb_triples(&ramp_triples()).unwrap();
        assert_eq!(p.entry(0), [0, 255, 0x55]);
        assert_eq!(p.entry(255), [255, 0, 255 ^ 0x55]);
    }

    #[test]
    fn from_tiff_colormap_validates_length() {
        assert!(Palette8::from_tiff_colormap(&[0u32; 767]).is_err());
        assert!(Palette8::from_tiff_colormap(&[0u32; 768]).is_ok());
    }

    #[test]
    fn colormap_roundtrip_is_lossless() {
        let p = Palette8::from_rgb_triples(&ramp_triples()).unwrap();
        // 8 -> 16 (×257) -> 8 (high byte) is the identity, so the palette survives a round-trip.
        let cm: Vec<u32> = p.to_tiff_colormap().iter().map(|&v| u32::from(v)).collect();
        let back = Palette8::from_tiff_colormap(&cm).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn colormap_layout_is_planar_rgb() {
        let p = Palette8::from_rgb_triples(&ramp_triples()).unwrap();
        let cm = p.to_tiff_colormap();
        // Reds occupy [0,256), greens [256,512), blues [512,768); ×257 scales 8-bit to 16-bit.
        // Entry 1: red=1, green=254, blue=(1^0x55), each ×257.
        assert_eq!(cm[1], 257);
        assert_eq!(cm[256 + 1], 254 * 257);
        assert_eq!(cm[512 + 1], u16::from(1u8 ^ 0x55) * 257);
    }
}
