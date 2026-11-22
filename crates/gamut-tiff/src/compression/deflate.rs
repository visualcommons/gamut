//! Adobe TIFF Deflate (`Compression = 8`), a complete zlib stream per strip or tile.
//!
//! Adobe Photoshop TIFF Technical Note 3 defines every image segment as an independent RFC 1950
//! zlib stream. Encoding uses the workspace's space-optimising [`gamut_deflate`]; bounded inflation
//! is its shared [`gamut_deflate::inflate_zlib`], as in `gamut-png` and `gamut-dng`. The
//! short-output check is this format's rule, so it stays here.

use gamut_core::{Error, Result};
use gamut_deflate::Level;

/// Compresses one already-packed strip or tile as a complete zlib stream at `level`
/// ([`TiffEncoder::with_deflate_level`](crate::TiffEncoder::with_deflate_level)).
pub fn encode(raw: &[u8], level: Level) -> Vec<u8> {
    let mut out = Vec::new();
    gamut_deflate::DeflateEncoder::new()
        .with_level(level)
        .zlib_compress(raw, &mut out);
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
        let zlib = encode(&raw, Level::Default);
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
        let mut zlib = encode(&[1, 2, 3, 4], Level::Default);
        zlib.pop();
        let truncated = decode(&zlib, 4).unwrap_err();
        assert_eq!(truncated.static_message(), Some("corrupt zlib stream"));
        assert_eq!(truncated.origin(), Some("gamut-tiff"));
        assert!(truncated.detail().is_some());
    }

    /// The level reaches the zlib encoder: every level inflates back exactly, and `Store` —
    /// stored blocks only — is strictly larger than `Best` on compressible data, so an `encode`
    /// that dropped `level` for the default cannot pass both bounds.
    #[test]
    fn every_level_round_trips_and_the_level_reaches_the_encoder() {
        let raw: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
        for level in [Level::Store, Level::Fast, Level::Default, Level::Best] {
            assert_eq!(
                decode(&encode(&raw, level), raw.len()).unwrap(),
                raw,
                "{level:?}"
            );
        }
        let stored = encode(&raw, Level::Store);
        assert!(
            stored.len() > raw.len(),
            "stored blocks add framing, never shrink"
        );
        assert!(encode(&raw, Level::Best).len() < stored.len());
        assert!(encode(&raw, Level::Default).len() < stored.len());
    }
}
