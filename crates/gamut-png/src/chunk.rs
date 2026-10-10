//! PNG chunk framing and the file signature (PNG spec §5).
//!
//! Every chunk is `length (u32 BE) || type (4 bytes) || data || CRC-32 (u32 BE)`, where the CRC
//! covers the type and data. All multi-byte integers in PNG are big-endian — the opposite of the
//! DEFLATE/zlib payload the IDAT chunks carry.

use core::ops::Range;

use gamut_core::{Error, Result};

use crate::crc32::Crc32;

/// The 8-byte PNG file signature (`\x89PNG\r\n\x1a\n`).
pub(crate) const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Appends a complete chunk (`length`, `type`, `data`, `CRC`) to `out`.
pub(crate) fn write_chunk(out: &mut Vec<u8>, chunk_type: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(&chunk_type);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// A chunk framed from the input stream (PNG spec §5.3).
pub(crate) struct RawChunk<'a> {
    /// The 4-byte chunk type.
    pub chunk_type: [u8; 4],
    /// The chunk's data payload.
    pub data: &'a [u8],
    /// Whether the stored CRC-32 (computed over type + data, §5.5) matched.
    pub crc_ok: bool,
    /// The chunk's whole span in the input, framing included: `12 + data.len()` bytes covering
    /// the length, type, payload and CRC fields. Single-sourced from the offset the reader
    /// already advances, so byte accounting cannot drift from framing.
    pub range: Range<usize>,
}

impl RawChunk<'_> {
    /// Whether the chunk is ancillary — bit 5 of the first type byte set, i.e. lowercase (§5.4).
    pub(crate) fn is_ancillary(&self) -> bool {
        self.chunk_type[0] & 0x20 != 0
    }
}

/// Iterates the chunks of a PNG stream after validating the signature (§5.2).
pub(crate) struct ChunkReader<'a> {
    rest: &'a [u8],
    offset: usize,
}

impl<'a> ChunkReader<'a> {
    /// Validates the 8-byte signature and positions the reader at the first chunk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the input does not start with the PNG signature.
    pub(crate) fn new(png: &'a [u8]) -> Result<Self> {
        match png.split_at_checked(SIGNATURE.len()) {
            Some((signature, rest)) if signature == SIGNATURE => Ok(Self {
                rest,
                offset: SIGNATURE.len(),
            }),
            _ => Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: bad signature")
                    .with_byte_offset(0),
            ),
        }
    }

    /// Frames the next chunk, or `None` at end of input.
    ///
    /// The CRC is verified but a mismatch is reported through [`RawChunk::crc_ok`] rather than as
    /// an error: the spec treats errors in ancillary chunks as recoverable (§13.1), so the chunk's
    /// criticality decides the response, and that is the caller's call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the chunk declares a length of 2³¹ or more (§5.3) or
    /// runs past the end of the input.
    pub(crate) fn next_chunk(&mut self) -> Result<Option<RawChunk<'a>>> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let offset = self.offset as u64;
        let (header, after) = self.rest.split_at_checked(8).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: truncated chunk")
                .with_byte_offset(offset)
        })?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        if length >= 1 << 31 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: chunk length exceeds 2^31 - 1",
            )
            .with_byte_offset(offset));
        }
        let chunk_type = [header[4], header[5], header[6], header[7]];
        let (data, tail) = after.split_at_checked(length as usize).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: chunk overruns the input")
                .with_byte_offset(offset)
        })?;
        let (stored, rest) = tail.split_at_checked(4).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: truncated chunk CRC")
                .with_byte_offset(offset)
        })?;
        let mut crc = Crc32::new();
        crc.update(&chunk_type);
        crc.update(data);
        let crc_ok = crc.finish().to_be_bytes() == stored;
        self.rest = rest;
        let start = self.offset;
        self.offset += 12 + length as usize;
        Ok(Some(RawChunk {
            chunk_type,
            data,
            crc_ok,
            range: start..self.offset,
        }))
    }

    /// The reader's cursor: the offset of the next chunk header, or — after [`next_chunk`] has
    /// returned an error — the start of the malformed one.
    ///
    /// [`next_chunk`]: Self::next_chunk
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iend_chunk_layout() {
        let mut out = Vec::new();
        write_chunk(&mut out, *b"IEND", &[]);
        // length 0, type "IEND", no data, fixed CRC 0xAE426082.
        assert_eq!(
            out,
            vec![0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]
        );
    }

    #[test]
    fn chunk_carries_length_and_data() {
        let mut out = Vec::new();
        write_chunk(&mut out, *b"tEXt", &[1, 2, 3]);
        assert_eq!(out[..4], 3u32.to_be_bytes()); // length field
        assert_eq!(&out[4..8], b"tEXt"); // type
        assert_eq!(&out[8..11], &[1, 2, 3]); // data
        assert_eq!(out.len(), 4 + 4 + 3 + 4); // + CRC
    }

    #[test]
    fn reader_round_trips_written_chunks() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"IHDR", &[1, 2, 3, 4, 5]);
        write_chunk(&mut png, *b"IEND", &[]);
        let mut reader = ChunkReader::new(&png).unwrap();
        let first = reader.next_chunk().unwrap().unwrap();
        assert_eq!(first.chunk_type, *b"IHDR");
        assert_eq!(first.data, &[1, 2, 3, 4, 5]);
        assert!(first.crc_ok);
        assert!(!first.is_ancillary());
        let second = reader.next_chunk().unwrap().unwrap();
        assert_eq!(second.chunk_type, *b"IEND");
        assert!(second.data.is_empty());
        assert!(reader.next_chunk().unwrap().is_none());
    }

    #[test]
    fn reader_rejects_bad_signature_and_truncation() {
        assert!(ChunkReader::new(&[]).is_err());
        assert!(ChunkReader::new(b"\x89PNG\r\n\x1a").is_err()); // one byte short
        let mut wrong = SIGNATURE;
        wrong[0] = 0x88;
        assert!(ChunkReader::new(&wrong).is_err());

        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"IHDR", &[0; 13]);
        // An empty chunk stream is end-of-input, not an error (missing IHDR is the decoder's
        // verdict); any partial chunk must error, never panic.
        let mut empty = ChunkReader::new(&png[..8]).unwrap();
        assert!(empty.next_chunk().unwrap().is_none());
        for cut in 9..png.len() {
            let mut reader = ChunkReader::new(&png[..cut]).unwrap();
            assert!(reader.next_chunk().is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn reader_flags_crc_mismatch_and_criticality() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"gAMA", &45455u32.to_be_bytes());
        let last = png.len() - 1;
        png[last] ^= 0xFF; // corrupt the CRC
        let mut reader = ChunkReader::new(&png).unwrap();
        let chunk = reader.next_chunk().unwrap().unwrap();
        assert!(!chunk.crc_ok);
        assert!(chunk.is_ancillary());
    }

    #[test]
    fn reader_rejects_oversized_length() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"tEXt", &[1, 2, 3]);
        png.extend_from_slice(&(1u32 << 31).to_be_bytes());
        png.extend_from_slice(b"IDAT");
        let mut reader = ChunkReader::new(&png).unwrap();
        assert!(reader.next_chunk().unwrap().is_some());
        let error = match reader.next_chunk() {
            Err(error) => error,
            Ok(_) => panic!("oversized chunk length must fail"),
        };
        assert_eq!(error.origin(), Some("gamut-png"));
        assert_eq!(error.byte_offset(), Some(23));
    }
}
