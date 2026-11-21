//! Compressing and decompressing the packed DNG sample stream.
//!
//! The codec wraps the [`crate::bitpack`] byte stream: the encoder packs samples then compresses
//! each strip; the decoder decompresses each strip then unpacks. DNG's **Deflate** (`Compression =
//! 8`) is zlib-format (RFC 1950), matching what the reference implementation reads/writes via
//! zlib's `compress2`/`uncompress`. Compression uses [`gamut_deflate`]'s encoder; decompression
//! uses its shared bounded [`gamut_deflate::inflate_zlib`], as `gamut-png` and `gamut-tiff` do.
//! Lossless JPEG and JPEG XL code samples rather than packed bytes, so they live in the encoder's
//! and decoder's own chunk coders, not here.

use gamut_core::{Error, Result};

use crate::values::Compression;

/// Whether `gamut-dng` can decode `scheme`: uncompressed, lossless JPEG, Deflate and JPEG XL.
/// Lossy JPEG is out of codec scope (see `STATUS.md`), and the TIFF-only schemes are not DNG's.
///
/// Every decodable scheme also encodes, with two caveats: [`Compression::JpegXl`] encoding needs
/// the `jxl-encode` cargo feature, and [`Compression::Deflate`] encoding is limited to 8/16-bit
/// samples.
#[must_use]
pub fn is_decodable(scheme: Compression) -> bool {
    matches!(
        scheme,
        Compression::None | Compression::Jpeg | Compression::Deflate | Compression::JpegXl
    )
}

/// Whether `scheme` stores the raw image **lossily**, which changes how its `NewRawImageDigest`
/// is computed: lossy storage digests the compressed chunks (the SDK's
/// `dng_lossy_compressed_image::FindDigest`) rather than the decoded samples. See
/// [`DngDecoder::verify_new_raw_image_digest`](crate::DngDecoder::verify_new_raw_image_digest).
///
/// JPEG XL counts as lossy storage for digest purposes whether or not the codestream itself was
/// encoded losslessly — the reference implementation routes every JXL image through the
/// compressed-chunk path.
#[must_use]
pub fn is_lossy(scheme: Compression) -> bool {
    matches!(scheme, Compression::LossyJpeg | Compression::JpegXl)
}

/// Deflates one already-packed strip to DNG's `Compression = 8` zlib stream.
///
/// Encoding goes through the workspace's own space-optimising zlib encoder at its default level
/// (lazy matching + per-block dynamic Huffman), the same choice `gamut-tiff` makes for Adobe
/// Deflate. Inflation is its shared bounded inflate — see [`decompress`]. Uncompressed strips are
/// the packed bytes themselves, and lossless JPEG and JPEG XL code samples rather than packed
/// bytes, so Deflate is the one scheme that compresses here and the caller selects it by calling
/// this function: no scheme parameter means no arm for a scheme it cannot encode.
pub(crate) fn deflate(packed: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    gamut_deflate::DeflateEncoder::new().zlib_compress(packed, &mut out);
    out
}

/// Decompresses one strip produced with `scheme` back to the packed sample bytes.
///
/// `max_out` is the packed byte length the chunk's geometry implies ([`crate::bitpack::packed_len`]).
/// A chunk can never legitimately inflate past it, so it doubles as the bound that stops a hostile
/// zlib stream from allocating without limit — the caller owns that cap, never the inflater.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
/// compressed data is malformed or inflates past `max_out`, or an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a scheme gamut-dng
/// cannot yet decode.
pub(crate) fn decompress(scheme: Compression, bytes: &[u8], max_out: usize) -> Result<Vec<u8>> {
    match scheme {
        Compression::None => Ok(bytes.to_vec()),
        Compression::Deflate => gamut_deflate::inflate_zlib(bytes, max_out, env!("CARGO_PKG_NAME")),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet decodable",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_lossy` splits the compressions that route through the compressed-chunk digest path
    /// from the ones that do not. Nothing asserted it, so it could be replaced by either constant
    /// (#110) -- which needs cases on both sides to close.
    #[test]
    fn is_lossy_names_the_compressed_chunk_compressions() {
        assert!(is_lossy(Compression::LossyJpeg));
        // JPEG XL counts as lossy storage whether or not its codestream was lossless.
        assert!(is_lossy(Compression::JpegXl));

        assert!(!is_lossy(Compression::None));
        assert!(!is_lossy(Compression::Jpeg));
        assert!(!is_lossy(Compression::Deflate));
    }

    /// Each decodable scheme is named separately (one per or-pattern alternative), and the
    /// out-of-scope lossy JPEG and a TIFF-only scheme are not decodable.
    #[test]
    fn is_decodable_names_the_four_decoded_schemes() {
        for scheme in [
            Compression::None,
            Compression::Jpeg,
            Compression::Deflate,
            Compression::JpegXl,
        ] {
            assert!(is_decodable(scheme), "{scheme:?}");
        }
        assert!(!is_decodable(Compression::LossyJpeg));
        assert!(!is_decodable(Compression::Lzw));
    }

    #[test]
    fn deflate_roundtrips() {
        let data: Vec<u8> = (0..4096).map(|i| (i * 7 % 251) as u8).collect();
        let packed = deflate(&data);
        assert!(packed.len() < data.len(), "structured data should shrink");
        assert_eq!(
            decompress(Compression::Deflate, &packed, data.len()).unwrap(),
            data
        );
    }

    /// A stream inflating to exactly `max_out` is accepted; one byte over is rejected, and with a
    /// message distinct from generic corruption so the caller can tell a bomb from a bad stream.
    #[test]
    fn inflation_is_capped_at_max_out() {
        let data: Vec<u8> = (0..4096).map(|i| (i * 7 % 251) as u8).collect();
        let packed = deflate(&data);
        assert!(decompress(Compression::Deflate, &packed, data.len()).is_ok());
        let over = decompress(Compression::Deflate, &packed, data.len() - 1).unwrap_err();
        assert_eq!(
            over.static_message(),
            Some("zlib stream inflates past the expected size")
        );
        assert_eq!(over.origin(), Some("gamut-dng"));
        assert!(over.detail().is_some());
    }

    #[test]
    fn corrupt_and_truncated_streams_are_rejected() {
        assert_eq!(
            decompress(Compression::Deflate, &[], 1)
                .unwrap_err()
                .static_message(),
            Some("corrupt zlib stream")
        );
        let mut packed = deflate(&[1, 2, 3, 4]);
        packed.pop();
        assert_eq!(
            decompress(Compression::Deflate, &packed, 4)
                .unwrap_err()
                .static_message(),
            Some("corrupt zlib stream")
        );
    }

    #[test]
    fn uncompressed_is_passthrough() {
        assert_eq!(
            decompress(Compression::None, &[1, 2, 3], 3).unwrap(),
            vec![1, 2, 3]
        );
    }
}
