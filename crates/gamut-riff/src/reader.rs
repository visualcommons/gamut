//! Reading the RIFF/WebP chunk structure: validate the 12-byte WebP file header, then iterate the
//! contained chunks (RFC 9649 §2.3-§2.4).

use gamut_core::{Error, Result};

use crate::chunk::{CHUNK_HEADER_LEN, Chunk, pad_len};
use crate::fourcc::FourCc;

/// Iterator over the top-level chunks of a WebP file's `RIFF`/`WEBP` payload.
///
/// Build one with [`RiffReader::new`], which validates the 12-byte WebP file header (`RIFF` + file
/// size + `WEBP`) and bounds the chunk region to the declared file size. Each [`Iterator::next`]
/// yields the next [`Chunk`], or an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error (after which iteration
/// ends) when the stream is truncated or a chunk's size runs past the available data.
#[derive(Debug, Clone)]
pub struct RiffReader<'a> {
    /// Remaining bytes, positioned at the next chunk header. Emptied once exhausted or on error.
    rest: &'a [u8],
    /// Offset of `rest` within the complete RIFF input.
    offset: usize,
    /// Bytes of the input past the region the file-size field declares. Fixed at construction.
    trailing: usize,
}

impl<'a> RiffReader<'a> {
    /// Parses the 12-byte WebP file header and returns a reader positioned at the first chunk.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data`
    /// is shorter than the 12-byte header, does not begin with the `RIFF` magic, has a form other
    /// than `WEBP`, or declares a file size that runs past the end of `data`.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        // 12-byte header: 'RIFF' (4) + file size (4, little-endian) + form (4).
        if data.len() < 12 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "RIFF: shorter than 12-byte file header",
            )
            .with_byte_offset(data.len() as u64));
        }
        if &data[0..4] != FourCc::RIFF.as_bytes() {
            return Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "RIFF: missing RIFF magic")
                    .with_byte_offset(0),
            );
        }
        if &data[8..12] != FourCc::WEBP.as_bytes() {
            return Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "RIFF: form is not WEBP")
                    .with_byte_offset(8),
            );
        }
        // file_size counts everything after the size field: the 'WEBP' form (4) plus the chunks. It
        // must cover at least the form and must not claim more bytes than are present.
        let file_size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        if file_size < 4 || file_size > data.len() - 8 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "RIFF: declared file size out of range",
            )
            .with_byte_offset(4));
        }
        // Chunks occupy bytes 12..(8 + file_size); ignore any trailing data past that point.
        Ok(Self {
            rest: &data[12..8 + file_size],
            offset: 12,
            trailing: data.len() - (8 + file_size),
        })
    }

    /// Bytes of the input that lie past the region the RIFF file-size field declares.
    ///
    /// The spec says a file "SHOULD NOT contain any data after the data specified by _File Size_",
    /// but that "readers MAY parse such files, ignoring the trailing data" (RFC 9649 §2.4). This
    /// reader takes the permissive option, so a non-zero count here is the only way to notice that
    /// the input was not exactly the file it claimed to be — useful for a strict caller, or for
    /// recovering an appended payload such as a motion-photo stream.
    #[must_use]
    pub const fn trailing_bytes(&self) -> usize {
        self.trailing
    }
}

impl<'a> Iterator for RiffReader<'a> {
    type Item = Result<Chunk<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.len() < CHUNK_HEADER_LEN {
            let offset = self.offset;
            self.rest = &[];
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "RIFF: truncated chunk header",
            )
            .with_byte_offset(offset as u64)));
        }
        let fourcc = FourCc([self.rest[0], self.rest[1], self.rest[2], self.rest[3]]);
        let size =
            u32::from_le_bytes([self.rest[4], self.rest[5], self.rest[6], self.rest[7]]) as usize;
        let avail = self.rest.len() - CHUNK_HEADER_LEN;
        if size > avail {
            let offset = self.offset;
            self.rest = &[];
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "RIFF: chunk size exceeds remaining data",
            )
            .with_byte_offset(offset as u64)));
        }
        let payload = &self.rest[CHUNK_HEADER_LEN..CHUNK_HEADER_LEN + size];
        let pad = pad_len(size as u32);
        // An odd payload is followed by a pad byte "which MUST be 0 to conform with RIFF"
        // (RFC 9649 §2.3). A non-conforming final chunk may omit it entirely — the `get` below
        // clamps for that — but a pad byte that is present and non-zero is malformed, and silently
        // skipping it would hide a byte an attacker controls.
        if let Some(&byte) = self.rest.get(CHUNK_HEADER_LEN + size)
            && pad == 1
            && byte != 0
        {
            let offset = self.offset + CHUNK_HEADER_LEN + size;
            self.rest = &[];
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "RIFF: chunk pad byte is not zero",
            )
            .with_byte_offset(offset as u64)));
        }
        // Advance past header + payload + the RIFF pad byte. A non-conforming final chunk may omit
        // the pad byte; `get` clamps so the reader simply ends instead of erroring on it.
        let consumed = CHUNK_HEADER_LEN + size + pad;
        self.rest = self.rest.get(consumed..).unwrap_or(&[]);
        self.offset += consumed;
        Some(Ok(Chunk { fourcc, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::RiffWriter;

    /// Builds a WebP file with the given chunks via the writer (the round-trip counterpart).
    fn build(chunks: &[(FourCc, &[u8])]) -> Vec<u8> {
        let mut w = RiffWriter::new();
        for (fourcc, payload) in chunks {
            w.write_chunk(*fourcc, payload).unwrap();
        }
        w.finish().unwrap()
    }

    #[test]
    fn roundtrips_multiple_chunks() {
        let file = build(&[
            (FourCc::VP8X, &[0xab; 10]),
            (FourCc::VP8L, &[1, 2, 3]), // odd → padded
            (FourCc::EXIF, &[0xee; 4]),
        ]);
        let got: Vec<Chunk> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            Chunk {
                fourcc: FourCc::VP8X,
                payload: &[0xab; 10]
            }
        );
        assert_eq!(
            got[1],
            Chunk {
                fourcc: FourCc::VP8L,
                payload: &[1, 2, 3][..]
            }
        );
        assert_eq!(
            got[2],
            Chunk {
                fourcc: FourCc::EXIF,
                payload: &[0xee; 4]
            }
        );
    }

    #[test]
    fn empty_chunk_list_yields_nothing() {
        let file = RiffWriter::new().finish().unwrap();
        assert_eq!(RiffReader::new(&file).unwrap().count(), 0);
    }

    #[test]
    fn rejects_short_header() {
        assert!(RiffReader::new(b"RIFF").is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut file = build(&[(FourCc::VP8L, &[0; 4])]);
        file[0] = b'X';
        assert!(RiffReader::new(&file).is_err());
    }

    #[test]
    fn rejects_non_webp_form() {
        let mut file = build(&[(FourCc::VP8L, &[0; 4])]);
        file[8] = b'A'; // corrupt 'WEBP'
        assert!(RiffReader::new(&file).is_err());
    }

    #[test]
    fn rejects_file_size_past_end() {
        let mut file = build(&[(FourCc::VP8L, &[0; 4])]);
        file[4..8].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert!(RiffReader::new(&file).is_err());
    }

    #[test]
    fn reads_final_empty_payload_chunk() {
        // A zero-payload final chunk leaves exactly CHUNK_HEADER_LEN bytes — the `<` boundary. It is
        // a complete header, so it must be read (not rejected as "truncated").
        let file = build(&[(FourCc::VP8L, &[])]);
        let chunks: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(
            chunks,
            vec![Chunk {
                fourcc: FourCc::VP8L,
                payload: &[][..]
            }]
        );
    }

    #[test]
    fn rejects_a_non_zero_pad_byte() {
        // The pad byte after an odd payload "MUST be 0 to conform with RIFF" (§2.3). A reader that
        // just skipped it would let an attacker smuggle a byte through every odd-sized chunk.
        let mut file = build(&[(FourCc::VP8L, &[1, 2, 3]), (FourCc::EXIF, &[4, 5])]);
        // 12 header + 8 chunk header + 3 payload = 23 is the pad byte's offset.
        assert_eq!(file[23], 0, "the writer emits a zero pad byte");
        assert!(
            RiffReader::new(&file).unwrap().all(|c| c.is_ok()),
            "the conforming file parses"
        );

        file[23] = 0xFF;
        let mut reader = RiffReader::new(&file).unwrap();
        // The pad byte belongs to the chunk that precedes it, so that chunk is what fails — the
        // reader never hands out a chunk whose framing it has already found to be malformed.
        let error = reader.next().unwrap().unwrap_err();
        assert_eq!(error.origin(), Some("gamut-riff"));
        assert_eq!(error.byte_offset(), Some(23));
        assert!(reader.next().is_none(), "iteration stops after an error");
    }

    #[test]
    fn tolerates_a_final_chunk_that_omits_its_pad_byte() {
        // A non-conforming writer may leave the trailing pad byte off the last chunk. There is no
        // byte to check, so this must still read cleanly — the pad-byte guard applies only to a pad
        // byte that is actually present.
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[1, 2, 3]).unwrap();
        let mut file = w.finish().unwrap();
        file.pop(); // drop the pad byte
        let new_size = u32::try_from(file.len() - 8).unwrap();
        file[4..8].copy_from_slice(&new_size.to_le_bytes());

        let chunks: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].payload, &[1, 2, 3]);
    }

    #[test]
    fn trailing_bytes_counts_data_past_the_declared_file_size() {
        let file = build(&[(FourCc::VP8L, &[0; 4])]);
        assert_eq!(
            RiffReader::new(&file).unwrap().trailing_bytes(),
            0,
            "a file that is exactly its declared size has no trailing data"
        );

        // Appending without touching the size field is how a motion-photo stream rides along. The
        // chunks still parse; the appended bytes are reported, not silently lost.
        let mut appended = file.clone();
        appended.extend_from_slice(b"appended payload");
        let reader = RiffReader::new(&appended).unwrap();
        assert_eq!(reader.trailing_bytes(), 16);
        assert_eq!(reader.count(), 1, "trailing data is not parsed as chunks");
    }

    #[test]
    fn errors_on_chunk_size_exceeding_data() {
        let mut file = build(&[(FourCc::VP8L, &[0; 4])]);
        // The chunk has 4 payload bytes available; declare 5 — one past the end. This pins the
        // `rest.len() - CHUNK_HEADER_LEN` available-bytes computation: a wrong sign would admit the
        // chunk and slice the payload out of bounds.
        file[16..20].copy_from_slice(&5u32.to_le_bytes());
        let mut reader = RiffReader::new(&file).unwrap();
        let error = reader.next().unwrap().unwrap_err();
        assert_eq!(error.origin(), Some("gamut-riff"));
        assert_eq!(error.byte_offset(), Some(12));
        assert!(reader.next().is_none(), "iteration stops after an error");
    }

    #[test]
    fn errors_on_truncated_trailing_header() {
        // A valid VP8L chunk followed by 3 stray bytes (too few for another header). Hand-build so
        // the RIFF file size includes the stray bytes.
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[0; 4]).unwrap();
        let mut file = w.finish().unwrap();
        file.extend_from_slice(&[1, 2, 3]);
        let new_size = u32::try_from(file.len() - 8).unwrap();
        file[4..8].copy_from_slice(&new_size.to_le_bytes());
        let results: Vec<_> = RiffReader::new(&file).unwrap().collect();
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        let error = results[1]
            .as_ref()
            .expect_err("trailing partial header is an error");
        assert_eq!(error.byte_offset(), Some(24));
    }
}
