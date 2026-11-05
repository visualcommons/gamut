//! PNG colour types and their bit-depth matrix (PNG spec §11.2.2, Table 12).

/// A PNG colour type, naming both the channel layout and its IHDR code.
///
/// `#[repr(u8)]` with the IHDR colour-type codes of §11.2.2 Table 12 as its discriminants, which
/// are permanent: the value crosses the C ABI as that plain integer. Closed, because the table
/// defines exactly these five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColorType {
    /// Greyscale: one channel. Valid bit depths 1, 2, 4, 8, 16.
    Grayscale = 0,
    /// Truecolour RGB: three channels. Valid bit depths 8, 16.
    Truecolor = 2,
    /// Indexed (palette): one channel of indices. Valid bit depths 1, 2, 4, 8.
    Indexed = 3,
    /// Greyscale with alpha: two channels. Valid bit depths 8, 16.
    GrayscaleAlpha = 4,
    /// Truecolour with alpha (RGBA): four channels. Valid bit depths 8, 16.
    TruecolorAlpha = 6,
}

impl ColorType {
    /// The IHDR colour-type code (0, 2, 3, 4, or 6): the discriminant.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The colour type for an IHDR colour-type code, or `None` if the code is not defined.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(ColorType::Grayscale),
            2 => Some(ColorType::Truecolor),
            3 => Some(ColorType::Indexed),
            4 => Some(ColorType::GrayscaleAlpha),
            6 => Some(ColorType::TruecolorAlpha),
            _ => None,
        }
    }

    /// Channels (samples) per pixel.
    #[must_use]
    pub fn channels(self) -> usize {
        match self {
            ColorType::Grayscale | ColorType::Indexed => 1,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Truecolor => 3,
            ColorType::TruecolorAlpha => 4,
        }
    }

    /// Whether `bit_depth` is permitted for this colour type (PNG Table 12).
    #[must_use]
    pub fn allows_bit_depth(self, bit_depth: u8) -> bool {
        match self {
            ColorType::Grayscale => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
            ColorType::Indexed => matches!(bit_depth, 1 | 2 | 4 | 8),
            ColorType::Truecolor | ColorType::GrayscaleAlpha | ColorType::TruecolorAlpha => {
                matches!(bit_depth, 8 | 16)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_channels() {
        assert_eq!(ColorType::Grayscale.code(), 0);
        assert_eq!(ColorType::Truecolor.code(), 2);
        assert_eq!(ColorType::Indexed.code(), 3);
        assert_eq!(ColorType::GrayscaleAlpha.code(), 4);
        assert_eq!(ColorType::TruecolorAlpha.code(), 6);
        assert_eq!(ColorType::Truecolor.channels(), 3);
        assert_eq!(ColorType::TruecolorAlpha.channels(), 4);
        assert_eq!(ColorType::GrayscaleAlpha.channels(), 2);
    }

    #[test]
    fn from_code_inverts_code() {
        for ty in [
            ColorType::Grayscale,
            ColorType::Truecolor,
            ColorType::Indexed,
            ColorType::GrayscaleAlpha,
            ColorType::TruecolorAlpha,
        ] {
            assert_eq!(ColorType::from_code(ty.code()), Some(ty));
        }
        for code in [1u8, 5, 7, 255] {
            assert_eq!(ColorType::from_code(code), None);
        }
    }

    #[test]
    fn bit_depth_matrix() {
        assert!(ColorType::Grayscale.allows_bit_depth(1));
        assert!(ColorType::Grayscale.allows_bit_depth(16));
        assert!(!ColorType::Truecolor.allows_bit_depth(4));
        assert!(ColorType::Truecolor.allows_bit_depth(8));
        assert!(!ColorType::Indexed.allows_bit_depth(16));
        assert!(ColorType::Indexed.allows_bit_depth(4));
    }
}
