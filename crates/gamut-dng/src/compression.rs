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

/// Compresses one already-packed strip with `scheme`.
///
/// # Errors
///
/// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a scheme
/// gamut-dng cannot yet encode.
pub(crate) fn compress(scheme: Compression, packed: &[u8]) -> Result<Vec<u8>> {
    match scheme {
        Compression::Uncompressed => Ok(packed.to_vec()),
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
        Compression::Uncompressed => Ok(bytes.to_vec()),
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
            compress(Compression::Uncompressed, &[1, 2, 3]).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            decompress(Compression::Uncompressed, &[1, 2, 3], 3).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn lossless_jpeg_not_yet_encodable() {
        assert!(compress(Compression::LosslessJpeg, &[0; 16]).is_err());
    }
}
