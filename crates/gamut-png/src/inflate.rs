//! Bounded zlib inflation (RFC 1950/1951) for the decoder: [`gamut_deflate::inflate_zlib`] bound
//! to this crate's origin.
//!
//! PNG compressed payloads (IDAT, iCCP, zTXt, compressed iTXt) are attacker-controlled, so every
//! inflation carries a hard output cap: a tiny stream claiming to inflate without bound (a "zlib
//! bomb") fails cleanly instead of exhausting memory. The inflate, its cap and its error
//! classification are the workspace's one shared copy in `gamut-deflate`; the short-output check
//! stays with each caller here (for example `PNG: IDAT is shorter than the image`).

use gamut_core::Result;

/// Inflates a zlib stream, refusing to produce more than `max_out` bytes. Errors carry this
/// crate's name as their [`origin`](gamut_core::Error::origin).
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the stream
/// is corrupt, truncated, or would inflate past `max_out`.
pub(crate) fn inflate_zlib(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    gamut_deflate::inflate_zlib(data, max_out, env!("CARGO_PKG_NAME"))
}

#[cfg(test)]
mod tests {
    use gamut_deflate::DeflateEncoder;

    use super::*;

    #[test]
    fn round_trips_gamut_deflate_output() {
        let payload: Vec<u8> = (0..2000u32).map(|i| (i * 7) as u8).collect();
        let mut zlib = Vec::new();
        DeflateEncoder::new().zlib_compress(&payload, &mut zlib);
        assert_eq!(inflate_zlib(&zlib, payload.len()).unwrap(), payload);
    }

    #[test]
    fn caps_output_size() {
        let payload = vec![0u8; 4096];
        let mut zlib = Vec::new();
        DeflateEncoder::new().zlib_compress(&payload, &mut zlib);
        // The stream is valid but inflates past the cap: that must be the *cap* error — the
        // distinction from plain corruption matters because a decoder reports a bomb, not a
        // damaged file.
        match inflate_zlib(&zlib, payload.len() - 1) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                assert_eq!(
                    error.static_message(),
                    Some("zlib stream inflates past the expected size"),
                    "cap-specific message"
                );
                assert_eq!(error.origin(), Some("gamut-png"));
                assert!(error.detail().is_some());
            }
            other => panic!("expected the cap error, got {other:?}"),
        }
        assert!(inflate_zlib(&zlib, payload.len()).is_ok());
    }

    /// Every non-cap failure is the *corrupt-stream* error, not the cap one, attributed to this
    /// crate rather than to `gamut-deflate`.
    fn assert_corrupt(result: Result<Vec<u8>>) {
        let error = result.expect_err("corrupt input must be rejected");
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("corrupt zlib stream"));
        assert_eq!(error.origin(), Some("gamut-png"));
        assert!(error.detail().is_some());
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert_corrupt(inflate_zlib(&[], 16));
        assert_corrupt(inflate_zlib(&[0xFF, 0xFF, 0x00], 16)); // bad zlib header
        let mut zlib = Vec::new();
        DeflateEncoder::new().zlib_compress(&[1, 2, 3, 4], &mut zlib);
        assert_corrupt(inflate_zlib(&zlib[..zlib.len() - 1], 16)); // truncated Adler-32
        let last = zlib.len() - 1;
        zlib[last] ^= 0xFF;
        assert_corrupt(inflate_zlib(&zlib, 16)); // wrong Adler-32
    }
}
