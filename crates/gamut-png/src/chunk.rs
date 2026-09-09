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

/// The C2PA manifest-store chunk type (C2PA 2.4 §A.3.2), spelled for its property bits (PNG
/// §5.4, Table 6): `c` ancillary, `a` private, `B` reserved bit clear, `X` **unsafe to copy**.
///
/// The last bit is the point. A PNG editor that rewrites the image must drop an unrecognised
/// unsafe-to-copy chunk (§14.2), and a C2PA manifest store is bound to the exact bytes it was
/// signed over (§18.5), so a store copied forward into a rewritten file is invalid by
/// construction. That is the same no-copy-forward law `gamut_metadata::C2paPolicy` states for
/// the facade, here enforced by the container's own naming convention — which is why the type is
/// spelled in exactly one place and its *bits* are asserted, not only its letters.
pub(crate) const CABX: [u8; 4] = *b"caBX";

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

/// Where a C2PA manifest store sits in a PNG: the `caBX` chunk's whole span and, inside it, the
/// store's own bytes. Reported by
/// [`PngEncoder::encode_with_report`](crate::PngEncoder::encode_with_report) for a file just
/// written and by [`PngReport::c2pa`](crate::PngReport::c2pa) for any file.
///
/// Non-exhaustive: a later revision may name a further range without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paSpan {
    /// The whole chunk — length, type, payload **and CRC**, `12 + payload` bytes. The range a
    /// `c2pa.hash.data` exclusion must cover (C2PA 2.4 §18.5.4): the store's bytes change when it
    /// is written, the length field when it is resized, and the CRC with either, so a hash that
    /// keeps any of them breaks on the store's first update.
    pub chunk: Range<usize>,
    /// The store's bytes alone — the chunk's payload, `chunk.start + 8 .. chunk.end - 4`. What a
    /// signer overwrites when it fills a reservation.
    pub payload: Range<usize>,
}

impl C2paSpan {
    /// The span of a `caBX` chunk occupying `chunk` (framing included), single-sourcing the
    /// framing arithmetic for both reports.
    pub(crate) fn of(chunk: Range<usize>) -> Self {
        Self {
            payload: chunk.start + 8..chunk.end - 4,
            chunk,
        }
    }
}

/// Locates the manifest store in a PNG: the first CRC-valid `caBX` chunk **before the first
/// `IDAT`**, or `None`.
///
/// The one definition of "the store", shared by every reader in this crate so they cannot
/// disagree — [`PngReport::c2pa`](crate::PngReport::c2pa) and the decoder's metadata walks apply
/// the same rule. Three parts, each load-bearing:
///
/// - **CRC-valid**, because §13.1 makes a mismatching ancillary chunk skippable, and the decoder
///   skips it — so a span a caller excludes from a hash names the store the decoder read;
/// - **the first**, because a PNG carries exactly one store (C2PA 2.4 §A.3.2); a later one is a
///   malformed file's extra chunk, never merged in;
/// - **before the first `IDAT`**, because §A.3.2 places the store there and calls data after
///   `IDAT` bad-form. A `caBX` appended to a finished file is therefore not the store, which is
///   what stops an appender turning a file that carries none into one that appears to.
///
/// The walk stops at the first `IDAT` or at `IEND`, whichever comes first: `IEND` ends the
/// datastream (§5.6), and bytes after it are a trailer, not chunks (§13.2). It also stops at the
/// first chunk that does not frame, so a stream that is not a PNG simply has no store.
pub(crate) fn find_c2pa(png: &[u8]) -> Option<C2paSpan> {
    let mut reader = ChunkReader::new(png).ok()?;
    while let Ok(Some(chunk)) = reader.next_chunk() {
        if chunk.chunk_type == *b"IDAT" || chunk.chunk_type == *b"IEND" {
            return None;
        }
        if chunk.chunk_type == CABX && chunk.crc_ok {
            return Some(C2paSpan::of(chunk.range));
        }
    }
    None
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

    /// The property bits of `caBX` (PNG §5.4, Table 6), asserted on the constant rather than on
    /// its letters: bit 5 of each byte is the property, and a typo that flips one — `cABX`, a
    /// public chunk; `caBx`, one an editor may copy forward — still reads as a plausible name.
    /// C2PA §A.3.2 requires ancillary, private and not safe to copy; PNG §5.4 requires the
    /// reserved bit clear. Note the polarity of the fourth byte: **clear** (uppercase) is unsafe
    /// to copy.
    #[test]
    fn cabx_property_bits_are_ancillary_private_reserved_clear_and_unsafe_to_copy() {
        const PROPERTY: u8 = 0x20;
        assert_ne!(CABX[0] & PROPERTY, 0, "byte 0: ancillary (lowercase)");
        assert_ne!(CABX[1] & PROPERTY, 0, "byte 1: private (lowercase)");
        assert_eq!(
            CABX[2] & PROPERTY,
            0,
            "byte 2: reserved bit clear (uppercase)"
        );
        assert_eq!(CABX[3] & PROPERTY, 0, "byte 3: unsafe to copy (uppercase)");
        assert_eq!(CABX, [0x63, 0x61, 0x42, 0x58]);
    }

    /// The span arithmetic at known offsets: a chunk at `33..52` (a 7-byte payload after the
    /// signature and IHDR) has its payload at `41..48`. Every byte of the framing is accounted
    /// — 4 length, 4 type ahead of the payload, 4 CRC behind it.
    #[test]
    fn a_c2pa_span_names_the_whole_chunk_and_the_payload_inside_it() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"IHDR", &[0; 13]);
        write_chunk(&mut png, CABX, b"jumbf!!");
        write_chunk(&mut png, *b"IEND", &[]);
        let span = find_c2pa(&png).expect("a caBX chunk");
        assert_eq!(span.chunk, 33..52);
        assert_eq!(span.payload, 41..48);
        assert_eq!(&png[span.payload.clone()], b"jumbf!!");
        assert_eq!(&png[span.chunk.start + 4..span.chunk.start + 8], b"caBX");
        // Nothing but the chunk: the span ends exactly where IEND's length field begins.
        assert_eq!(&png[span.chunk.end + 4..span.chunk.end + 8], b"IEND");
    }

    /// The walk ends with the datastream. A `caBX` after `IDAT` is bad-form carriage (C2PA
    /// §A.3.2) and one after `IEND` is not in the datastream at all (§13.2) — neither is the
    /// store, so an appender cannot inject one into a file that carries none.
    #[test]
    fn find_c2pa_stops_at_the_first_idat_and_at_iend() {
        let mut after_idat = SIGNATURE.to_vec();
        write_chunk(&mut after_idat, *b"IHDR", &[0; 13]);
        write_chunk(&mut after_idat, *b"IDAT", b"zz");
        write_chunk(&mut after_idat, CABX, b"appended");
        write_chunk(&mut after_idat, *b"IEND", &[]);
        assert_eq!(find_c2pa(&after_idat), None, "a caBX after IDAT is not the store");

        let mut after_iend = SIGNATURE.to_vec();
        write_chunk(&mut after_iend, *b"IHDR", &[0; 13]);
        write_chunk(&mut after_iend, *b"IEND", &[]);
        write_chunk(&mut after_iend, CABX, b"trailing");
        assert_eq!(find_c2pa(&after_iend), None, "a caBX after IEND is not the store");

        // ...while the same chunk one position earlier — before IDAT — is the store, so the
        // stop is what decides, not the payload.
        let mut before_idat = SIGNATURE.to_vec();
        write_chunk(&mut before_idat, *b"IHDR", &[0; 13]);
        write_chunk(&mut before_idat, CABX, b"appended");
        write_chunk(&mut before_idat, *b"IDAT", b"zz");
        write_chunk(&mut before_idat, *b"IEND", &[]);
        let span = find_c2pa(&before_idat).expect("a store before IDAT");
        assert_eq!(&before_idat[span.payload], b"appended");
    }

    /// The store the span names is the one the decoder reads: a `caBX` whose CRC does not match
    /// is skipped on decode (§13.1), so it is skipped here too, and the CRC-valid one after it
    /// is the store. A stream with no `caBX`, or no signature, has none.
    #[test]
    fn find_c2pa_skips_a_crc_mismatch_and_names_the_first_valid_store() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"IHDR", &[0; 13]);
        write_chunk(&mut png, CABX, b"corrupt");
        let last = png.len() - 1;
        png[last] ^= 0xFF; // the first store's CRC no longer matches
        let valid_start = png.len();
        write_chunk(&mut png, CABX, b"valid");
        write_chunk(&mut png, *b"IEND", &[]);
        let span = find_c2pa(&png).expect("the CRC-valid caBX");
        assert_eq!(span.chunk, valid_start..valid_start + 12 + 5);
        assert_eq!(&png[span.payload], b"valid");

        let mut none = SIGNATURE.to_vec();
        write_chunk(&mut none, *b"IHDR", &[0; 13]);
        write_chunk(&mut none, *b"IEND", &[]);
        assert_eq!(find_c2pa(&none), None);
        assert_eq!(find_c2pa(b"not a png"), None);
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
