//! RIFF chunk layout: an 8-byte header (FourCC + `uint32` little-endian size) followed by a payload
//! that is padded to an even length with a single zero byte (RFC 9649 §2.3; Google *WebP
//! Container*, "RIFF File Format").

use crate::fourcc::FourCc;

/// Size of a chunk header (FourCC + size field), in bytes.
///
/// Internal: the header is framing the reader and writer own, never something a caller assembles.
/// A caller who needs a chunk's on-disk span can compute it as
/// `CHUNK_HEADER_LEN + payload.len() + pad_len(..)` — but no consumer ever has, so neither constant
/// is part of the v1 surface.
pub(crate) const CHUNK_HEADER_LEN: usize = 8;

/// Byte length of a RIFF/WebP file header: the `RIFF` magic, the `uint32` file size, and the `WEBP`
/// form type (RFC 9649 §2.4). Chunks begin here.
pub(crate) const RIFF_HEADER_LEN: usize = 12;

/// Number of pad bytes that follow a payload of `size` bytes: RIFF pads an odd-sized payload to an
/// even boundary with a single zero byte, so this is `1` for odd `size` and `0` otherwise.
pub(crate) const fn pad_len(size: u32) -> usize {
    (size & 1) as usize
}

/// A borrowed RIFF chunk: its FourCC and a slice over its payload (excluding header and padding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The chunk's four-character code.
    pub fourcc: FourCc,
    /// The chunk payload, excluding the header and any pad byte.
    pub payload: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_one_for_odd_sizes() {
        assert_eq!(pad_len(0), 0);
        assert_eq!(pad_len(4), 0);
        assert_eq!(pad_len(1), 1);
        assert_eq!(pad_len(7), 1);
    }
}
