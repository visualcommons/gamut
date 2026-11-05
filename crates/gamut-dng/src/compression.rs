//! Compressing and decompressing the packed DNG sample stream.
//!
//! The codec wraps the [`crate::bitpack`] byte stream: the encoder packs samples then compresses
//! each strip; the decoder decompresses each strip then unpacks. DNG's **Deflate** (`Compression =
//! 8`) is zlib-format (RFC 1950), matching what the reference implementation reads/writes via
//! zlib's `compress2`/`uncompress`. Compression uses [`gamut_deflate`]; decompression uses
//! [`miniz_oxide`], since `gamut-deflate` is deliberately encoder-only. Lossless JPEG and JPEG XL
//! are added in later phases.

use gamut_core::{Error, Result};
use miniz_oxide::inflate::TINFLStatus;

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

/// Compresses one already-packed strip with `scheme`.
///
/// # Errors
///
/// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a scheme
/// gamut-dng cannot yet encode.
pub(crate) fn compress(scheme: Compression, packed: &[u8]) -> Result<Vec<u8>> {
    match scheme {
        Compression::None => Ok(packed.to_vec()),
        // Encoding goes through the workspace's own space-optimising zlib encoder at its default
        // level (lazy matching + per-block dynamic Huffman), the same choice `gamut-tiff` makes for
        // Adobe Deflate. Inflation stays with `miniz_oxide` — see [`decompress`].
        Compression::Deflate => {
            let mut out = Vec::new();
            gamut_deflate::DeflateEncoder::new().zlib_compress(packed, &mut out);
            Ok(out)
        }
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet encodable",
        )),
    }
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
        Compression::Deflate => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
            bytes, max_out,
        )
        .map_err(|error| {
            let classified = match error.status {
                TINFLStatus::HasMoreOutput => Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: Deflate stream inflates past the expected size",
                ),
                _ => Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: corrupt Deflate stream"),
            };
            classified.with_detail(format!("miniz status {:?}", error.status))
        }),
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
        let packed = compress(Compression::Deflate, &data).unwrap();
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
        let packed = compress(Compression::Deflate, &data).unwrap();
        assert!(decompress(Compression::Deflate, &packed, data.len()).is_ok());
        let over = decompress(Compression::Deflate, &packed, data.len() - 1).unwrap_err();
        assert_eq!(
            over.static_message(),
            Some("DNG: Deflate stream inflates past the expected size")
        );
        assert!(over.detail().is_some());
    }

    #[test]
    fn corrupt_and_truncated_streams_are_rejected() {
        assert_eq!(
            decompress(Compression::Deflate, &[], 1)
                .unwrap_err()
                .static_message(),
            Some("DNG: corrupt Deflate stream")
        );
        let mut packed = compress(Compression::Deflate, &[1, 2, 3, 4]).unwrap();
        packed.pop();
        assert_eq!(
            decompress(Compression::Deflate, &packed, 4)
                .unwrap_err()
                .static_message(),
            Some("DNG: corrupt Deflate stream")
        );
    }

    #[test]
    fn uncompressed_is_passthrough() {
        assert_eq!(
            compress(Compression::None, &[1, 2, 3]).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            decompress(Compression::None, &[1, 2, 3], 3).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn lossless_jpeg_not_yet_encodable() {
        assert!(compress(Compression::Jpeg, &[0; 16]).is_err());
    }
}
