//! TIFF compression schemes, selected by the `Compression` tag (259).
//!
//! Baseline TIFF readers must handle the uncompressed, Modified Huffman, and PackBits schemes;
//! the remainder are extensions (TIFF 6.0 Part 2). Each scheme is decoded/encoded per strip or
//! tile by the crate-internal per-scheme codecs (Deflate, LZW, PackBits, CCITT, and the differencing
//! predictor); [`Compression`] — gamut-ifd's shared code, re-exported from this crate's root —
//! selects the scheme on the public encoder/decoder surface.

// The per-scheme codecs are implementation details of the encoder/decoder (they operate on raw
// packed strip/tile bytes); every scheme is reachable through `Compression` on the public API.
pub(crate) mod ccitt;
pub(crate) mod deflate;
pub(crate) mod lzw;
pub(crate) mod packbits;
pub(crate) mod predictor;

use gamut_core::Result;
pub(crate) use gamut_ifd::tags::Compression;

/// Reads an on-disk `Compression` code (tag 259) as one of the schemes TIFF assigns.
///
/// The legacy Adobe Deflate code `32946` reads as [`Compression::Deflate`], like the standardised
/// `8`. The DNG-only codes ([`Compression::LossyJpeg`], [`Compression::JpegXl`]) are not TIFF's
/// and are refused here like any unassigned code, so [`TiffInfo`](crate::TiffInfo) and
/// [`deconstruct`](crate::deconstruct) keep treating them as unrecognised.
///
/// # Errors
///
/// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a code outside
/// TIFF's set.
pub(crate) fn tiff_compression(code: u32) -> Result<Compression> {
    crate::ifd::read(
        code,
        |compression| {
            matches!(
                compression,
                Compression::None
                    | Compression::CcittRle
                    | Compression::CcittGroup3Fax
                    | Compression::CcittGroup4Fax
                    | Compression::Lzw
                    | Compression::OldJpeg
                    | Compression::Jpeg
                    | Compression::Deflate
                    | Compression::PackBits
            )
        },
        "TIFF: unrecognised Compression tag value",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TIFF reads exactly its own codes (TIFF 6.0 §8 and Technical Notes 2 and 3) plus the legacy
    /// Deflate alias; the DNG-only codes, a code past `u16`, and unassigned codes are refused.
    #[test]
    fn tiff_compression_reads_only_tiffs_codes() {
        for (code, c) in [
            (1, Compression::None),
            (2, Compression::CcittRle),
            (3, Compression::CcittGroup3Fax),
            (4, Compression::CcittGroup4Fax),
            (5, Compression::Lzw),
            (6, Compression::OldJpeg),
            (7, Compression::Jpeg),
            (8, Compression::Deflate),
            (32773, Compression::PackBits),
            (32946, Compression::Deflate),
        ] {
            assert_eq!(tiff_compression(code).unwrap(), c, "{code}");
        }
        for bad in [0u32, 9, 99, 32774, 34892, 52546, 65536 + 1] {
            assert_eq!(
                tiff_compression(bad).unwrap_err().kind(),
                gamut_core::ErrorKind::Unsupported,
                "{bad}"
            );
        }
    }
}
