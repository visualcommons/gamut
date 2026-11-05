//! TIFF compression schemes, selected by the `Compression` tag (259).
//!
//! Baseline TIFF readers must handle the uncompressed, Modified Huffman, and PackBits schemes;
//! the remainder are extensions (TIFF 6.0 Part 2). Each scheme is decoded/encoded per strip or
//! tile by the crate-internal per-scheme codecs (Deflate, LZW, PackBits, CCITT, and the differencing
//! predictor); [`Compression`] selects the scheme on the public encoder/decoder surface.

// The per-scheme codecs are implementation details of the encoder/decoder (they operate on raw
// packed strip/tile bytes); every scheme is reachable through `Compression` on the public API.
pub(crate) mod ccitt;
pub(crate) mod deflate;
pub(crate) mod lzw;
pub(crate) mod packbits;
pub(crate) mod predictor;

/// A compression scheme applied to a strip or tile of image data.
///
/// The discriminants are documented with their on-disk `Compression` tag values. The set is
/// non-exhaustive: TIFF registers many further schemes (and post-6.0 extensions keep adding
/// them), so recognised codes may be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Compression {
    /// `1` — no compression; samples are packed into bytes as tightly as possible.
    #[default]
    None,
    /// `2` — CCITT Group 3 1-Dimensional Modified Huffman run-length encoding (TIFF 6.0 §10).
    CcittRle,
    /// `3` — CCITT T.4 (Group 3) bilevel fax encoding (TIFF 6.0 §11).
    CcittGroup3Fax,
    /// `4` — CCITT T.6 (Group 4) bilevel fax encoding (TIFF 6.0 §11).
    CcittGroup4Fax,
    /// `5` — LZW (TIFF 6.0 §13).
    Lzw,
    /// `6` — the deprecated old-style JPEG process (TIFF 6.0 §22).
    OldJpeg,
    /// `7` — JPEG (the redefined "new-style" process; TIFF Technical Note 2).
    Jpeg,
    /// `8` — Deflate using the zlib data format (Adobe TIFF Technical Note 3).
    Deflate,
    /// `32773` — PackBits, a simple byte-oriented run-length scheme (TIFF 6.0 §9).
    PackBits,
}

impl TryFrom<u32> for Compression {
    type Error = gamut_core::Error;

    /// Maps an on-disk `Compression` tag value (tag 259) to its scheme.
    ///
    /// The legacy Adobe Deflate code `32946` maps to [`Compression::Deflate`] like the
    /// standardised `8`. Unrecognised codes fail with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        Ok(match code {
            1 => Compression::None,
            2 => Compression::CcittRle,
            3 => Compression::CcittGroup3Fax,
            4 => Compression::CcittGroup4Fax,
            5 => Compression::Lzw,
            6 => Compression::OldJpeg,
            7 => Compression::Jpeg,
            8 | 32946 => Compression::Deflate,
            32773 => Compression::PackBits,
            _ => {
                return Err(gamut_core::Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unrecognised Compression tag value",
                ));
            }
        })
    }
}

impl From<Compression> for u16 {
    /// Returns the on-disk `Compression` tag value (the `SHORT` written to tag 259).
    fn from(compression: Compression) -> Self {
        match compression {
            Compression::None => 1,
            Compression::CcittRle => 2,
            Compression::CcittGroup3Fax => 3,
            Compression::CcittGroup4Fax => 4,
            Compression::Lzw => 5,
            Compression::OldJpeg => 6,
            Compression::Jpeg => 7,
            Compression::Deflate => 8,
            Compression::PackBits => 32773,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_codes_round_trip() {
        // Both conversion directions agree, per on-disk code (TIFF 6.0 §7 Compression values).
        for (c, code) in [
            (Compression::None, 1u16),
            (Compression::CcittRle, 2),
            (Compression::CcittGroup3Fax, 3),
            (Compression::CcittGroup4Fax, 4),
            (Compression::Lzw, 5),
            (Compression::OldJpeg, 6),
            (Compression::Jpeg, 7),
            (Compression::Deflate, 8),
            (Compression::PackBits, 32773),
        ] {
            assert_eq!(u16::from(c), code);
            assert_eq!(Compression::try_from(u32::from(code)).unwrap(), c);
        }
        // The legacy Adobe Deflate code is an accepted read-side alias.
        assert_eq!(Compression::try_from(32946).unwrap(), Compression::Deflate);
        for bad in [0u32, 9, 99, 32774] {
            assert!(Compression::try_from(bad).is_err());
        }
    }
}
