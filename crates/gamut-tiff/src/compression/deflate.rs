//! Adobe TIFF Deflate (`Compression = 8`), a complete zlib stream per strip or tile.
//!
//! Adobe Photoshop TIFF Technical Note 3 defines every image segment as an independent RFC 1950
//! zlib stream. Encoding uses the workspace's space-optimising [`gamut_deflate`]; bounded inflation
//! is its shared [`gamut_deflate::inflate_zlib`], as in `gamut-png` and `gamut-dng`. The
//! short-output check is this format's rule, so it stays here.

use gamut_core::{Error, Result};

/// Compresses one already-packed strip or tile as a complete zlib stream.
pub fn encode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    gamut_deflate::DeflateEncoder::new().zlib_compress(raw, &mut out);
    out
}

/// Inflates one zlib stream to exactly `expected` bytes.
pub fn decode(zlib: &[u8], expected: usize) -> Result<Vec<u8>> {
    let decoded = gamut_deflate::inflate_zlib(zlib, expected, env!("CARGO_PKG_NAME"))?;
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
            Some("zlib stream inflates past the expected size")
        );
        assert_eq!(too_long.origin(), Some("gamut-tiff"));
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
            Some("corrupt zlib stream")
        );
        let mut zlib = encode(&[1, 2, 3, 4]);
        zlib.pop();
        let truncated = decode(&zlib, 4).unwrap_err();
        assert_eq!(truncated.static_message(), Some("corrupt zlib stream"));
        assert_eq!(truncated.origin(), Some("gamut-tiff"));
        assert!(truncated.detail().is_some());
    }
}
