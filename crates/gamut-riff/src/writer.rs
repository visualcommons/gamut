//! Writing the RIFF/WebP chunk structure: a 12-byte `RIFF`/size/`WEBP` header followed by padded
//! chunks (RFC 9649 §2.3-§2.4).

use gamut_core::{Error, Result};

use crate::chunk::pad_len;
use crate::fourcc::FourCc;

/// The largest value the RIFF file-size field may hold: "the maximum value of this field is 2^32
/// minus 10 bytes, and thus the size of the whole file is at most 4 GiB minus 2 bytes"
/// (RFC 9649 §2.4).
const MAX_FILE_SIZE: u64 = (1 << 32) - 10;

/// Builder for a RIFF/WebP byte stream.
///
/// Begin with [`RiffWriter::new`] (which emits the `RIFF` magic, a placeholder file size, and the
/// `WEBP` form), append chunks with [`RiffWriter::write_chunk`], then call [`RiffWriter::finish`] to
/// back-patch the file size and obtain the finished bytes.
#[derive(Debug, Clone)]
pub struct RiffWriter {
    /// Accumulated output. Bytes `4..8` hold the file-size placeholder patched in `finish`.
    buf: Vec<u8>,
}

impl RiffWriter {
    /// Creates a writer for a WebP file, emitting the 12-byte `RIFF` / size / `WEBP` header with the
    /// size left as a placeholder to be patched in [`RiffWriter::finish`].
    #[must_use]
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(FourCc::RIFF.as_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]); // file-size placeholder, patched in finish()
        buf.extend_from_slice(FourCc::WEBP.as_bytes());
        Self { buf }
    }

    /// Appends one chunk: its FourCC, the `uint32` little-endian payload size, the payload, and a
    /// single zero pad byte when the payload length is odd (RFC 9649 §2.3).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if `payload`
    /// is longer than `u32::MAX`, which the chunk's `uint32` size field cannot express. Such a
    /// payload cannot occur for a still image; rejecting it keeps an over-large one from being
    /// silently written with a truncated size.
    pub fn write_chunk(&mut self, fourcc: FourCc, payload: &[u8]) -> Result<()> {
        let size = chunk_size_field(payload.len())?;
        self.buf.extend_from_slice(fourcc.as_bytes());
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(payload);
        if pad_len(size) == 1 {
            self.buf.push(0);
        }
        Ok(())
    }

    /// Back-patches the RIFF file-size field (the byte count following the 8-byte `RIFF`+size
    /// prefix: the `WEBP` form plus all chunks) and returns the finished byte stream.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the
    /// accumulated file exceeds the spec's ceiling of `2^32 - 10` bytes for the file-size field
    /// (RFC 9649 §2.4).
    pub fn finish(mut self) -> Result<Vec<u8>> {
        // `buf` always holds at least the 12-byte header, so the subtraction cannot underflow.
        let file_size = file_size_field(self.buf.len() as u64 - 8)?;
        self.buf[4..8].copy_from_slice(&file_size.to_le_bytes());
        Ok(self.buf)
    }
}

/// Narrows a payload length to a chunk's `uint32` size field (RFC 9649 §2.3).
///
/// Split out of [`RiffWriter::write_chunk`] so the limit can be tested without allocating the 4 GiB
/// payload needed to reach it.
fn chunk_size_field(payload_len: usize) -> Result<u32> {
    u32::try_from(payload_len).map_err(|_| {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "RIFF: chunk payload exceeds the uint32 size field",
        )
    })
}

/// Narrows an accumulated byte count to the RIFF file-size field, rejecting one past the spec's
/// `2^32 - 10` ceiling (RFC 9649 §2.4).
///
/// Split out of [`RiffWriter::finish`] so the ceiling can be tested without allocating a 4 GiB
/// buffer to reach it.
fn file_size_field(file_size: u64) -> Result<u32> {
    if file_size > MAX_FILE_SIZE {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "RIFF: file exceeds the 2^32 - 10 byte size limit",
        ));
    }
    // The bound above guarantees the cast is exact.
    Ok(file_size as u32)
}

impl Default for RiffWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn header_is_riff_placeholder_webp() {
        let w = RiffWriter::new();
        assert_eq!(&w.buf[0..4], b"RIFF");
        assert_eq!(&w.buf[4..8], &[0, 0, 0, 0]);
        assert_eq!(&w.buf[8..12], b"WEBP");
    }

    #[test]
    fn even_payload_has_no_pad_byte() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[1, 2, 3, 4]).unwrap();
        let out = w.finish().unwrap();
        // 12 header + 8 chunk header + 4 payload, no pad.
        assert_eq!(out.len(), 12 + 8 + 4);
        assert_eq!(&out[12..16], b"VP8L");
        assert_eq!(&out[16..20], &4u32.to_le_bytes());
    }

    #[test]
    fn odd_payload_gets_one_zero_pad_byte() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8, &[9, 8, 7]).unwrap();
        let out = w.finish().unwrap();
        // 12 header + 8 chunk header + 3 payload + 1 pad.
        assert_eq!(out.len(), 12 + 8 + 3 + 1);
        assert_eq!(*out.last().unwrap(), 0, "odd payload must be zero-padded");
        // Size field records the *unpadded* length.
        assert_eq!(&out[16..20], &3u32.to_le_bytes());
    }

    #[test]
    fn finish_patches_file_size() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[0; 6]).unwrap();
        let out = w.finish().unwrap();
        let file_size = u32::from_le_bytes([out[4], out[5], out[6], out[7]]) as usize;
        assert_eq!(
            file_size,
            out.len() - 8,
            "file size counts everything after the size field"
        );
        assert_eq!(file_size & 1, 0, "WebP file size is always even");
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(
            RiffWriter::default().finish().unwrap(),
            RiffWriter::new().finish().unwrap()
        );
    }

    #[test]
    fn file_size_field_admits_the_ceiling_and_rejects_one_past_it() {
        // Pins the boundary exactly: `>` must not be `>=` (which would reject the largest legal
        // file) nor `<` (which would admit every over-large one).
        assert_eq!(file_size_field(MAX_FILE_SIZE).unwrap(), 0xFFFF_FFF6);
        assert_eq!(file_size_field(0).unwrap(), 0);

        let error = file_size_field(MAX_FILE_SIZE + 1).expect_err("one byte past the ceiling");
        assert_eq!(error.origin(), Some("gamut-riff"));
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    // `u32::MAX as usize + 1` is not representable on a 32-bit target (wasm32 is a CI cross
    // target), where the guard is unreachable because no slice can be that long.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn chunk_size_field_admits_u32_max_and_rejects_one_past_it() {
        assert_eq!(chunk_size_field(u32::MAX as usize).unwrap(), u32::MAX);

        let error = chunk_size_field(u32::MAX as usize + 1).expect_err("past the uint32 field");
        assert_eq!(error.origin(), Some("gamut-riff"));
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}
