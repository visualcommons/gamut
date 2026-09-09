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
/// written and by [`PngReport::c2pa`](crate::PngReport::c2pa) for any file, and consumed by
/// [`fill_c2pa`] to write the finished store into it.
///
/// A span describes **carriage** — where the bytes sit — and says nothing about whether a decode
/// surfaced them: [`PngDecoder::with_max_metadata_bytes`](crate::PngDecoder::with_max_metadata_bytes)
/// can skip a store this span still names, since a byte accounting has no budget and does not
/// borrow another reader's. Exclude the span from a hash; read the payload from the decode.
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

/// Writes a finished C2PA manifest store into the `caBX` chunk `span` names, **in place**.
///
/// The second half of the reserve-then-fill flow (C2PA 2.4 §18.5): encode once with
/// [`PngEncoder::with_c2pa_reserved`](crate::PngEncoder::with_c2pa_reserved), hash the output with
/// `span.chunk` excluded, have the signer build a store of exactly the reserved length, then call
/// this. Only the payload and the chunk's CRC change; the length field, the type, and every byte
/// outside `span.chunk` — every offset in the file — are untouched, so the hash the signer signed
/// still describes the filled file.
///
/// **This is the supported way to put a store into a file this encoder is not re-encoding**, and
/// the only one for a file gamut did not write. Re-encoding with
/// [`PngEncoder::with_c2pa`](crate::PngEncoder::with_c2pa) reaches the same bytes, but it costs a
/// second full encode (at [`Level::Best`](crate::Level) with
/// [`FilterStrategy::BruteForce`](crate::FilterStrategy) that is the whole brute-force set again)
/// and it makes the signature depend on the encoder reproducing its output byte for byte. Filling
/// in place depends on nothing but these twelve-plus-`n` bytes.
///
/// The span comes from [`PngEncodeReport::c2pa`](crate::PngEncodeReport::c2pa) for a file this
/// encoder just wrote, or from [`PngReport::c2pa`](crate::PngReport::c2pa) for any file — which is
/// also the route for an indexed image, since
/// [`encode_indexed8`](crate::PngEncoder::encode_indexed8) has no report of its own.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] and leaves `png` **unmodified** if `span` runs past the end of
/// `png`, if it does not frame a chunk (its payload must be `chunk.start + 8 .. chunk.end - 4`),
/// if the bytes it names are not a `caBX` chunk, or if `store` is not exactly the reserved
/// length. Every check runs before the first byte is written, so a rejected call cannot leave a
/// half-filled chunk behind — and a store of the wrong length is rejected rather than resized,
/// because resizing would move every byte after the chunk and invalidate the signer's hash.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::{PngEncoder, fill_c2pa};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let (mut png, report) = PngEncoder::new()
///     .with_c2pa_reserved(16)
///     .encode_with_report(image)?;
/// let span = report.c2pa.expect("a reservation was made");
///
/// // ... hash `png` with `span.chunk` excluded, sign, and receive a 16-byte store ...
/// fill_c2pa(&mut png, &span, &[7u8; 16])?;
///
/// assert_eq!(gamut_png::metadata(&png)?.c2pa.as_deref(), Some(&[7u8; 16][..]));
/// # Ok(())
/// # }
/// ```
pub fn fill_c2pa(png: &mut [u8], span: &C2paSpan, store: &[u8]) -> Result<()> {
    let invalid = |message: &'static str| Error::invalid_input(env!("CARGO_PKG_NAME"), message);
    if span.chunk.end > png.len() {
        return Err(invalid("PNG: the C2PA span runs past the end of the image"));
    }
    // The span must frame a chunk: 4 length bytes and 4 type bytes ahead of the payload, 4 CRC
    // bytes behind it. Checked rather than assumed because a caller can build a `C2paSpan`.
    let frames = span
        .chunk
        .start
        .checked_add(8)
        .zip(span.chunk.end.checked_sub(4))
        .is_some_and(|(payload_start, payload_end)| {
            span.payload.start == payload_start
                && span.payload.end == payload_end
                && payload_start <= payload_end
        });
    if !frames {
        return Err(invalid("PNG: the C2PA span does not frame a chunk"));
    }
    if png[span.chunk.start + 4..span.payload.start] != CABX {
        return Err(invalid("PNG: the C2PA span does not name a caBX chunk"));
    }
    if store.len() != span.payload.len() {
        return Err(invalid("PNG: the C2PA store is not the reserved length"));
    }

    png[span.payload.clone()].copy_from_slice(store);
    let mut crc = Crc32::new();
    crc.update(&CABX);
    crc.update(store);
    png[span.payload.end..span.chunk.end].copy_from_slice(&crc.finish().to_be_bytes());
    Ok(())
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

    /// A PNG carrying a `len`-byte reservation, plus the span naming it.
    fn reserved(len: usize) -> (Vec<u8>, C2paSpan) {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, *b"IHDR", &[0; 13]);
        write_chunk(&mut png, CABX, &vec![0; len]);
        write_chunk(&mut png, *b"IDAT", b"zz");
        write_chunk(&mut png, *b"IEND", &[]);
        let span = find_c2pa(&png).expect("a reservation");
        (png, span)
    }

    /// Filling rewrites the payload and the CRC, and nothing else: every byte outside the span
    /// is untouched, the length and type inside it are untouched, and the chunk still frames —
    /// `find_c2pa` re-reads the filled store, which it can only do if the CRC was recomputed.
    #[test]
    fn filling_a_reservation_rewrites_the_payload_and_its_crc_alone() {
        let (mut png, span) = reserved(6);
        let before = png.clone();
        fill_c2pa(&mut png, &span, b"jumbf!").expect("fill");

        assert_eq!(&png[span.payload.clone()], b"jumbf!");
        assert_eq!(png.len(), before.len());
        for i in (0..png.len()).filter(|i| !span.chunk.contains(i)) {
            assert_eq!(png[i], before[i], "byte {i} outside the span changed");
        }
        assert_eq!(
            png[span.chunk.start..span.payload.start],
            before[span.chunk.start..span.payload.start],
            "the length and type fields are untouched"
        );
        assert_ne!(
            png[span.payload.end..span.chunk.end],
            before[span.payload.end..span.chunk.end],
            "the CRC followed the payload"
        );
        // The CRC is not merely different, it is right: the walk only returns a CRC-valid chunk.
        let refound = find_c2pa(&png).expect("the filled chunk still verifies");
        assert_eq!(refound, span);
        assert_eq!(&png[refound.payload], b"jumbf!");
    }

    /// Every argument is validated before a byte is written, each with its own message, and a
    /// rejected call leaves the image exactly as it was.
    #[test]
    fn filling_validates_its_span_and_length_before_writing() {
        let (png, span) = reserved(4);

        let mut short = png.clone();
        let error = fill_c2pa(&mut short, &span, b"abc").expect_err("one byte short");
        assert!(
            error.to_string().contains("not the reserved length"),
            "{error}"
        );
        assert_eq!(short, png, "a rejected fill writes nothing");
        let mut long = png.clone();
        assert!(
            fill_c2pa(&mut long, &span, b"abcde").is_err(),
            "one byte long"
        );
        assert_eq!(long, png);

        // A span past the end of the buffer.
        let mut truncated = png[..span.chunk.end - 1].to_vec();
        let error = fill_c2pa(&mut truncated, &span, b"abcd").expect_err("past the end");
        assert!(error.to_string().contains("runs past the end"), "{error}");

        // A span whose payload does not sit inside its framing.
        let mut mine = png.clone();
        let skewed = C2paSpan {
            chunk: span.chunk.clone(),
            payload: span.payload.start + 1..span.payload.end,
        };
        let error = fill_c2pa(&mut mine, &skewed, b"abc").expect_err("not framed");
        assert!(
            error.to_string().contains("does not frame a chunk"),
            "{error}"
        );
        assert_eq!(mine, png);

        // A well-framed span naming some other chunk: the IHDR right before it.
        let ihdr = C2paSpan::of(8..8 + 12 + 13);
        let error = fill_c2pa(&mut mine, &ihdr, &[0; 13]).expect_err("not a caBX");
        assert!(
            error.to_string().contains("does not name a caBX"),
            "{error}"
        );
        assert_eq!(mine, png);
    }

    /// The bounds check admits the exact fit: a buffer that ends exactly where the chunk does is
    /// in range, not past it. `fill_c2pa` takes a `&mut [u8]`, so a caller may legitimately hand
    /// it the prefix of a file up to the end of the store — and a file whose store happens to be
    /// its last chunk is the same shape. Off by one here and every such call is refused.
    #[test]
    fn a_buffer_ending_exactly_where_the_chunk_does_is_in_range() {
        let (mut png, span) = reserved(4);
        let end = span.chunk.end;
        fill_c2pa(&mut png[..end], &span, b"abcd").expect("the chunk ends at the buffer's end");
        assert_eq!(&png[span.payload], b"abcd");
    }

    /// A zero-length reservation is a legal chunk, and filling it with nothing is a no-op that
    /// still verifies — the boundary where payload start and end coincide.
    #[test]
    fn filling_an_empty_reservation_is_lawful() {
        let (mut png, span) = reserved(0);
        assert_eq!(span.payload.len(), 0);
        fill_c2pa(&mut png, &span, &[]).expect("fill");
        assert_eq!(find_c2pa(&png), Some(span));
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
        assert_eq!(
            find_c2pa(&after_idat),
            None,
            "a caBX after IDAT is not the store"
        );

        let mut after_iend = SIGNATURE.to_vec();
        write_chunk(&mut after_iend, *b"IHDR", &[0; 13]);
        write_chunk(&mut after_iend, *b"IEND", &[]);
        write_chunk(&mut after_iend, CABX, b"trailing");
        assert_eq!(
            find_c2pa(&after_iend),
            None,
            "a caBX after IEND is not the store"
        );

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
