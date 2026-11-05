//! Adobe TIFF Deflate (`Compression = 8`), a complete zlib stream per strip or tile.
//!
//! Adobe Photoshop TIFF Technical Note 3 defines every image segment as an independent RFC 1950
//! zlib stream. Encoding uses the workspace's space-optimising [`gamut_deflate`]; bounded inflation
//! delegates the security-sensitive decode surface to [`miniz_oxide`], as `gamut-png` does.

use gamut_core::{Error, Result};
use miniz_oxide::inflate::TINFLStatus;

/// Compresses one already-packed strip or tile as a complete zlib stream.
pub fn encode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    gamut_deflate::DeflateEncoder::new().zlib_compress(raw, &mut out);
    out
}

/// Inflates one zlib stream to exactly `expected` bytes.
pub fn decode(zlib: &[u8], expected: usize) -> Result<Vec<u8>> {
    let decoded = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(zlib, expected).map_err(
        |error| {
            let classified = match error.status {
                TINFLStatus::HasMoreOutput => Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: Deflate stream exceeds the expected size",
                ),
                _ => Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: corrupt Deflate stream"),
            };
            classified.with_detail(format!("miniz status {:?}", error.status))
        },
    )?;
    if decoded.len() != expected {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: Deflate stream shorter than expected",
        ));
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_round_trip_and_size_errors_are_distinct() {
        let raw: Vec<u8> = (0..4096u32).map(|i| (i * 17) as u8).collect();
        let zlib = encode(&raw);
        assert_eq!(decode(&zlib, raw.len()).unwrap(), raw);
        let too_long = decode(&zlib, raw.len() - 1).unwrap_err();
        assert_eq!(
            too_long.static_message(),
            Some("TIFF: Deflate stream exceeds the expected size")
        );
        assert!(too_long.detail().is_some());
        assert_eq!(
            decode(&zlib, raw.len() + 1).unwrap_err().static_message(),
            Some("TIFF: Deflate stream shorter than expected")
        );
    }

    #[test]
    fn corruption_and_truncation_are_rejected() {
        assert_eq!(
            decode(&[], 1).unwrap_err().static_message(),
            Some("TIFF: corrupt Deflate stream")
        );
        let mut zlib = encode(&[1, 2, 3, 4]);
        zlib.pop();
        let truncated = decode(&zlib, 4).unwrap_err();
        assert_eq!(
            truncated.static_message(),
            Some("TIFF: corrupt Deflate stream")
        );
        assert!(truncated.detail().is_some());
    }
}
