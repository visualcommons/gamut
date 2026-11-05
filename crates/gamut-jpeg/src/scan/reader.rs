//! The entropy-coded-segment reader both scan types decode through: the byte-destuffing,
//! marker-aware bit reader (§F.2.2.5, restart handling per §E.2.5) and the canonical `DECODE`
//! (Figure F.16) procedure, alone or fused with `RECEIVE`/`EXTEND` (Figures F.17, F.12).

use std::num::{NonZeroI32, NonZeroU16};

use gamut_core::{Error, Result};

use crate::huffman::{DecTable, LOOKAHEAD, extend};

/// The outcome of fetching one destuffed entropy byte.
enum Fetch {
    /// A data byte (any stuffed `0x00` after `0xFF` already consumed).
    Data(u8),
    /// A marker was reached; the reader's `marker`/`marker_off` are set and `pos` is frozen.
    Marker,
    /// The input ended without a terminating marker (truncated stream).
    Eof,
}

/// A most-significant-bit-first reader over the entropy-coded segment, applying §F.2.2.5 byte
/// destuffing (`0xFF 0x00` → literal `0xFF`) and stopping cleanly at any real marker (fill `0xFF`
/// bytes before a marker per §B.1.1.2 are skipped). Once a marker is reached, further bit requests
/// are satisfied with `1`-padding so the final valid symbols still decode; the byte cursor stays
/// parked on the marker.
///
/// The buffer is 64 bits wide and [`Self::fill`] tops it up four bytes at a time to more than 32
/// bits, so a read is a shift and a mask rather than a byte fetch per request, and a Huffman code
/// and the magnitude bits after it can be resolved from a single 32-bit [`Self::peek`] (issues
/// #305, #897). Prefetching runs ahead of what
/// the decoder has consumed, and is invisible to every caller: whatever it appends past the
/// entropy data -- `1`-padding at a marker, zero bytes at the end of a truncated stream -- is
/// counted in [`Self::pad_bits`], and [`Self::consume`] is where reaching into it is judged, so a
/// truncation is reported at the read that needs the missing bits, exactly as a byte-at-a-time
/// reader would.
pub(super) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u64,
    bit_count: u32,
    /// How many of the low `bit_count` buffered bits were appended past the entropy data rather
    /// than read from it: `1`-padding past a marker, or zero bytes past the end of a stream that
    /// has no marker. Padding only ever follows the last data byte, so it is always the low end of
    /// the buffer, and a stream reaches at most one of the two kinds.
    pad_bits: u32,
    /// Whether a read has consumed marker padding: the decoder needed more bits than the entropy
    /// data held before its marker.
    overread: bool,
    marker: Option<u8>,
    marker_off: usize,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(data: &'a [u8], start: usize) -> Self {
        Self {
            data,
            pos: start,
            bit_buf: 0,
            bit_count: 0,
            pad_bits: 0,
            overread: false,
            marker: None,
            marker_off: start,
        }
    }

    /// Fetches the next destuffed data byte, or reports a marker / EOF (§F.2.2.5).
    ///
    /// A run of `0xFF` bytes is fill before a marker (§B.1.1.2) except its last, which pairs with
    /// the byte after it. The run is measured rather than stepped over one byte at a time, so this
    /// has no loop of its own, and the cursor is parked on the run's last `0xFF` -- the marker's
    /// first byte -- whatever follows it.
    ///
    /// The rewrite's cost in the survey (#924): the fill-byte step `pos += 1` it replaced carried
    /// two mutants (`-=`, `*=`), both TIMEOUTs and neither killable, so no killable sibling went
    /// with it; the run count adds five (`fetch` went from 10 mutants to 13).
    fn fetch(&mut self) -> Fetch {
        if self.marker.is_some() {
            return Fetch::Marker;
        }
        let Some(&b) = self.data.get(self.pos) else {
            return Fetch::Eof;
        };
        if b != 0xFF {
            self.pos += 1;
            return Fetch::Data(b);
        }
        self.pos += self.data[self.pos + 1..]
            .iter()
            .take_while(|&&b| b == 0xFF)
            .count();
        match self.data.get(self.pos + 1) {
            None => Fetch::Eof,
            Some(0x00) => {
                self.pos += 2;
                Fetch::Data(0xFF)
            }
            Some(&m) => {
                self.marker = Some(m);
                self.marker_off = self.pos;
                Fetch::Marker
            }
        }
    }

    /// Tops the buffer up once 32 or fewer bits are left in it, four whole bytes at a time (eight
    /// when it is empty): entropy data while there is some, then `1`-padding past a marker or zero
    /// bytes past the end of a truncated stream, both counted in [`Self::pad_bits`]. Afterwards at
    /// least 33 bits are buffered, so any read of up to 32 bits can be [`Self::peek`]ed, and a
    /// buffer holding more than 32 is left alone -- most calls, one per coefficient, do nothing.
    ///
    /// When the next `room` bytes are all in the input and none of them is `0xFF`, they are plain
    /// entropy data -- no stuffing to undo, no marker to stop at -- and are loaded as one big-endian
    /// word (zune-jpeg's `BitStream::refill` does the same four bytes at a time). Otherwise the
    /// bytes go through [`Self::fetch`] one at a time. Both paths append the same bytes; the first
    /// only skips the per-byte classification.
    ///
    /// The byte count is arithmetic in the buffered count -- `(64 − bit_count) / 32` words -- rather
    /// than a threshold comparison, whose off-by-one mutant would only refill one call early and
    /// so could not be told from the original. It is computed before the loop rather than
    /// re-tested on each pass, so the loop is bounded by its own arithmetic whatever the body does.
    ///
    /// Each byte is composed with `+` rather than `|`: the shift just vacated 8 low zero bits and
    /// the addend is `< 256`, so the addition fills exactly those vacant bits (the same convention
    /// as `bitwriter`/`pack_nibbles`, where `|` and `^` would be equivalent on disjoint operands).
    /// The word path shifts in two halves of `4 · room` because a single shift by `8 · room` is out
    /// of range for a `u64` when the buffer is empty (`room = 8`).
    fn fill(&mut self) {
        let room = (64 - self.bit_count) / 32 * 4;
        if room == 0 {
            return;
        }
        let end = self.pos + room as usize;
        if let Some(bytes) = self.data.get(self.pos..end)
            && !bytes.contains(&0xFF)
        {
            let mut word = [0u8; 8];
            word[8 - bytes.len()..].copy_from_slice(bytes);
            self.bit_buf = (self.bit_buf << (4 * room) << (4 * room)) + u64::from_be_bytes(word);
            self.pos = end;
            self.bit_count += room * 8;
            return;
        }
        for _ in 0..room {
            let byte = match self.fetch() {
                Fetch::Data(b) => b,
                Fetch::Marker => {
                    self.pad_bits += 8;
                    0xFF
                }
                Fetch::Eof => {
                    self.pad_bits += 8;
                    0x00
                }
            };
            self.bit_buf = (self.bit_buf << 8) + u64::from(byte);
        }
        self.bit_count += room * 8;
    }

    /// The next `n` (`1..=32`) buffered bits, MSB-first, without consuming them. Valid after
    /// [`Self::fill`], which leaves at least 33 bits buffered.
    fn peek(&self, n: u32) -> u32 {
        ((self.bit_buf >> (self.bit_count - n)) & ((1u64 << n) - 1)) as u32
    }

    /// Consumes the next `n` buffered bits. Reaching past the entropy data is allowed at a marker
    /// (the `1`-padding the final symbols decode from, recorded as an overread) and is a truncated
    /// stream otherwise -- the error a byte-at-a-time reader raises when its fetch finds no byte.
    pub(super) fn consume(&mut self, n: u32) -> Result<()> {
        let data_bits = self.bit_count - self.pad_bits;
        if n > data_bits {
            if self.marker.is_none() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: truncated entropy-coded data",
                )
                .with_byte_offset(self.pos as u64));
            }
            // Past the data, padding is all there is: what remains buffered is no longer told
            // apart, because `at_data_end` answers from `overread` alone from here on.
            self.overread = true;
            self.pad_bits = 0;
        }
        self.bit_count -= n;
        Ok(())
    }

    /// Reads `n` (`0..=16`) bits MSB-first (`RECEIVE`, Figure F.17).
    pub(super) fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.fill();
        let bits = self.peek(n);
        self.consume(n)?;
        Ok(bits)
    }

    /// Reads one bit (the `NEXTBIT` of Figure F.16).
    pub(super) fn read_bit(&mut self) -> Result<i32> {
        self.fill();
        let bit = self.peek(1);
        self.consume(1)?;
        Ok(bit as i32)
    }

    /// Whether, at an MCU-row boundary, only `1`-padding remains before a marker — the end of the
    /// entropy data for a `Y = 0` (DNL) frame whose MCU-row count is not known in advance. In the
    /// middle of a scan the pending bits are real data (or the next byte is not a marker), so this is
    /// `false`; a false positive would require the pending bits to be all-`1` *and* the next byte to
    /// begin a marker, which coincide only at the true end.
    ///
    /// "Pending" means bits the decoder has not consumed and that came from entropy data: the
    /// prefetched padding past a marker is not pending, and a decoder that has already consumed
    /// padding is past the end by definition.
    pub(super) fn at_data_end(&self) -> bool {
        if self.overread {
            return true;
        }
        let pending = self.bit_count - self.pad_bits;
        if pending >= 8 {
            return false; // a whole unconsumed data byte is buffered
        }
        let mask = (1u64 << pending) - 1;
        if ((self.bit_buf >> self.pad_bits) & mask) != mask {
            return false; // pending bits are not all-1 padding
        }
        // Peek forward for a marker without consuming (mirrors `fetch`): skip any run of fill 0xFF
        // bytes (§B.1.1.2) and classify the byte that follows.
        let fill = self
            .data
            .get(self.pos..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&b| b == 0xFF)
            .count();
        match self.data.get(self.pos + fill) {
            // No 0xFF prefix at all: still inside the destuffed data byte stream.
            _ if fill == 0 => false,
            // Truncated, or a stuffed `0xFF 0x00` (literal data), or a restart marker: not the end.
            // A restart (RSTm) is a mid-scan byte-aligned boundary — a complete scan never emits a
            // trailing RST, so an RSTm always precedes more MCUs (§E.2.5). Treating it as the end
            // would truncate a Y=0 frame whose restart interval lands on an MCU-row start.
            None | Some(0x00) => false,
            Some(&m) if (0xD0..=0xD7).contains(&m) => false,
            // Any other marker (EOI, DNL, …) terminates the entropy data.
            Some(_) => true,
        }
    }

    /// Byte-aligns by discarding everything buffered: the partial byte the decoder was in, and any
    /// whole bytes prefetched past it, which the marker search below would skip as data anyway.
    fn discard_buffer(&mut self) {
        self.bit_count = 0;
        self.bit_buf = 0;
        self.pad_bits = 0;
        self.overread = false;
    }

    /// Consumes a restart marker at an interval boundary (§E.2.5): byte-aligns, verifies the marker
    /// is `RSTm` with `m == expected`, and advances past it. The caller resets the DC predictors.
    pub(super) fn take_restart(&mut self, expected: u8) -> Result<()> {
        self.discard_buffer();
        let Some(code) = self.next_marker() else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: truncated before restart marker",
            ));
        };
        if !(0xD0..=0xD7).contains(&code) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: expected restart marker",
            ));
        }
        if code - 0xD0 != expected {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: restart marker out of sequence",
            ));
        }
        self.pos = self.marker_off + 2;
        self.marker = None;
        Ok(())
    }

    /// Byte-aligns and advances to the marker that ends the scan, returning `(code, offset)` without
    /// consuming it (the segment parser resumes at `offset`).
    pub(super) fn end_marker(&mut self) -> Result<(u8, usize)> {
        self.discard_buffer();
        match self.next_marker() {
            Some(code) => Ok((code, self.marker_off)),
            None => Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: missing marker after scan",
            )),
        }
    }

    /// Skips entropy data up to the next marker and returns its code, or `None` when the input
    /// ends first.
    ///
    /// Every [`Self::fetch`] that returns data consumes at least one byte, so the bytes left bound
    /// the walk: it is a `for` over them rather than a `loop` that ends only when the cursor's own
    /// arithmetic delivers it to a marker. A cursor advance that stops advancing therefore runs out
    /// of steps and reports the input exhausted, instead of re-reading one byte forever.
    fn next_marker(&mut self) -> Option<u8> {
        for _ in self.pos..=self.data.len() {
            match self.fetch() {
                Fetch::Data(_) => {}
                Fetch::Marker => return self.marker,
                Fetch::Eof => return None,
            }
        }
        None
    }
}

/// `DECODE` (Figure F.16): resolves the canonical code at the head of the bit stream, returning its
/// symbol.
///
/// The procedure runs over a 16-bit [`BitReader::peek`] -- one probe of the table's precomputed
/// [`DecTable::lookup`] for codes of up to [`LOOKAHEAD`] bits, [`DecTable::walk`] otherwise -- and
/// then consumes exactly the bits it read. A stream that ends inside the code therefore fails in
/// [`BitReader::consume`], as reading those bits one at a time would.
pub(super) fn decode_symbol(table: &DecTable, r: &mut BitReader) -> Result<u8> {
    r.fill();
    let (length, symbol) = resolve(table, r.peek(16));
    r.consume(length)?;
    symbol
}

/// The Figure F.16 outcome for the code at the head of the MSB-first 16-bit window `bits`: how
/// many bits the code takes, and its symbol or the error a corrupt code earns. One probe of
/// [`DecTable::lookup`] for codes of up to [`LOOKAHEAD`] bits, [`DecTable::walk`] otherwise.
fn resolve(table: &DecTable, bits: u32) -> (u32, Result<u8>) {
    // A miss is told apart by `NonZeroU16`, not by comparing the entry with zero: inverting that
    // comparison would send misses to a zero-length "hit" that consumes nothing, which a Y=0 frame
    // can only report as a hang, never as a failure.
    match NonZeroU16::new(table.lookup(bits >> (16 - LOOKAHEAD))) {
        Some(entry) => (u32::from(entry.get() >> 8), Ok((entry.get() & 0xFF) as u8)),
        None => table.walk(bits),
    }
}

/// `DECODE` (Figure F.16) of a run/size or DC-category symbol, together with the `EXTEND`ed value
/// (Figure F.12) of the magnitude bits that follow its code -- from one [`BitReader::fill`] and one
/// 32-bit [`BitReader::peek`] -- returning `(symbol, value)`.
///
/// It consumes only the code, exactly as [`decode_symbol`] does: the caller validates the symbol
/// against its context (a DC category above 11, an AC run past the band) and then consumes the
/// `symbol & 0x0F` magnitude bits with [`BitReader::consume`]. A corrupt or truncated stream
/// therefore fails at the same read, with the same error, as `DECODE` followed by `RECEIVE`.
/// `value` is `0` when that size is `0`, and is meaningful only once those bits have been consumed.
///
/// One probe of [`DecTable::fused`] resolves a code and magnitude that together fit in
/// [`LOOKAHEAD`] bits; otherwise the code resolves as in [`decode_symbol`] and its magnitude is
/// sliced from the same window. A code is at most 16 bits and a size at most 15, so the 32-bit
/// window always holds both.
pub(super) fn decode_coded(table: &DecTable, r: &mut BitReader) -> Result<(u8, i32)> {
    r.fill();
    let window = r.peek(32);
    // A miss is told apart by `NonZeroI32` for the reason `resolve` gives.
    if let Some(entry) = NonZeroI32::new(table.fused(window >> (32 - LOOKAHEAD))) {
        let entry = entry.get();
        r.consume((entry & 0xFF) as u32)?;
        return Ok((((entry >> 8) & 0xFF) as u8, entry >> 16));
    }
    let (length, symbol) = resolve(table, window >> 16);
    r.consume(length)?;
    let symbol = symbol?;
    let size = symbol & 0x0F;
    let value = if size == 0 {
        0
    } else {
        let magnitude = (window << length) >> (32 - u32::from(size));
        extend(magnitude, size)
    };
    Ok((symbol, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bits of `bytes`, MSB-first: the reference the buffered reader must reproduce.
    fn bit(bytes: &[u8], i: usize) -> u32 {
        u32::from(bytes[i / 8] >> (7 - i % 8) & 1)
    }

    /// `BitReader::fill`/`peek`/`read_bits`: reads of every width from 1 to 16 bits, in a sequence
    /// that crosses each refill at a different offset, return exactly the stream's bits. A refill
    /// that took one byte too many would shift the oldest buffered bit out of the 64-bit word; one
    /// that took too few, or miscounted, would misalign every later read.
    #[test]
    fn bit_reader_reads_match_the_stream_across_refills() {
        let bytes: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8 & 0xFE).collect();
        let mut r = BitReader::new(&bytes, 0);
        let mut at = 0usize;
        for n in (1..=16u32).cycle().take(40) {
            let want = (0..n as usize).fold(0, |v, k| v * 2 + bit(&bytes, at + k));
            assert_eq!(r.read_bits(n).unwrap(), want, "{n} bits at bit {at}");
            at += n as usize;
        }
        // `read_bit` takes the next single bit, and zero-width reads take nothing.
        assert_eq!(r.read_bits(0).unwrap(), 0);
        assert_eq!(r.read_bit().unwrap() as u32, bit(&bytes, at));
    }

    /// `BitReader::consume`: a stream that ends without a marker supplies exactly its own bits. The
    /// read that ends on the last one succeeds; the next fails as truncated, at the stream's end.
    #[test]
    fn bit_reader_truncation_fails_at_the_first_missing_bit() {
        let mut r = BitReader::new(&[0xAB, 0xCD], 0);
        assert_eq!(r.read_bits(12).unwrap(), 0xABC);
        assert_eq!(r.read_bits(4).unwrap(), 0xD);
        let err = r.read_bit().unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("JPEG: truncated entropy-coded data")
        );
        assert_eq!(err.byte_offset(), Some(2));
    }

    /// `BitReader::consume` at a marker: reads past the data return `1`-padding instead of failing,
    /// and from then on `at_data_end` reports the end whatever follows.
    #[test]
    fn bit_reader_pads_with_ones_past_a_marker() {
        let mut r = BitReader::new(&[0x12, 0xFF, 0xD0], 0);
        assert_eq!(r.read_bits(8).unwrap(), 0x12);
        // Exactly at the data's end, before a restart marker: not the end of the scan.
        assert!(!r.at_data_end());
        assert_eq!(r.read_bits(5).unwrap(), 0x1F);
        assert!(r.at_data_end(), "an overread is the end, even before RSTm");
    }

    /// `BitReader::at_data_end` reads only the pending *data* bits: prefetched marker padding is
    /// not pending, a pending partial byte must be all ones, a whole pending byte means data
    /// remains, and the marker that follows must end the scan (not be a restart).
    #[test]
    fn bit_reader_at_data_end_reads_only_pending_data_bits() {
        let end_after = |bytes: &[u8], n: u32| {
            let mut r = BitReader::new(bytes, 0);
            r.read_bits(n).unwrap();
            r.at_data_end()
        };
        // Four pending bits, all ones, then EOI.
        assert!(end_after(&[0xAF, 0xFF, 0xD9], 4));
        // Four pending bits, not all ones.
        assert!(!end_after(&[0xA7, 0xFF, 0xD9], 4));
        // A whole pending byte of ones (a stuffed 0xFF) is data, not padding.
        assert!(!end_after(&[0x00, 0xFF, 0x00, 0xFF, 0xD9], 8));
        // All-ones partial byte, but the marker is a restart: more MCUs follow.
        assert!(!end_after(&[0xAF, 0xFF, 0xD3], 4));
        // Nothing read yet: a data byte is next.
        assert!(!BitReader::new(&[0x00, 0xFF, 0xD9], 0).at_data_end());
    }

    /// `decode_symbol`: codes resolved by the lookahead table and codes longer than it decode alike,
    /// and each consumes exactly its own length. The table has one code of every length 1..=16
    /// except the all-ones code of length 16, so the stream walks both paths in turn.
    #[test]
    fn decode_symbol_consumes_exactly_each_codes_length() {
        let bits = [1u8; 16];
        let values: Vec<u8> = (1..=16).collect();
        let table = DecTable::from_bits(&bits, &values).unwrap();
        // Code of length l is l−1 ones then a zero; length 16 is fifteen ones then a zero.
        let mut stream = Vec::new();
        for l in [1usize, 9, 10, 3, 16, 12, 2] {
            stream.extend(std::iter::repeat_n(1u8, l - 1));
            stream.push(0);
        }
        stream.extend([0, 1, 0, 1]); // trailing raw bits, read after the last code
        let mut packed = vec![0u8; stream.len().div_ceil(8)];
        for (i, &b) in stream.iter().enumerate() {
            packed[i / 8] += b << (7 - i % 8);
        }
        // Byte-stuff (§F.1.2.3): the long runs of ones form whole 0xFF bytes.
        let mut bytes = Vec::new();
        for b in packed {
            bytes.push(b);
            if b == 0xFF {
                bytes.push(0x00);
            }
        }
        bytes.extend([0xFF, 0xD9]);
        let mut r = BitReader::new(&bytes, 0);
        for l in [1u8, 9, 10, 3, 16, 12, 2] {
            assert_eq!(decode_symbol(&table, &mut r).unwrap(), l);
        }
        assert_eq!(r.read_bits(4).unwrap(), 0b0101);
    }

    /// `decode_symbol` on a code the table cannot resolve: sixteen ones match no code of this
    /// table (its all-ones length-16 slot is unassigned), the Figure F.16 undecodable case.
    #[test]
    fn decode_symbol_rejects_an_unassigned_code() {
        let bits = [1u8; 16];
        let values: Vec<u8> = (1..=16).collect();
        let table = DecTable::from_bits(&bits, &values).unwrap();
        let mut r = BitReader::new(&[0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00], 0);
        assert_eq!(
            decode_symbol(&table, &mut r).unwrap_err().static_message(),
            Some("JPEG: undecodable Huffman code")
        );
    }

    /// `BitReader::fill` on a stream whose stuffed `0xFF 0x00` pairs recur at an interval the
    /// refills do not divide: each refill's span sometimes holds one (the byte-at-a-time path) and
    /// sometimes not (the one-word path), and every read still returns the destuffed stream's bits.
    #[test]
    fn bit_reader_word_and_byte_refills_read_the_same_stream() {
        let data: Vec<u8> = (0..96u32)
            .map(|i| {
                if i % 11 == 5 {
                    0xFF
                } else {
                    (i * 37 + 11) as u8 & 0xFE
                }
            })
            .collect();
        let mut bytes = Vec::new();
        for &b in &data {
            bytes.push(b);
            if b == 0xFF {
                bytes.push(0x00);
            }
        }
        let mut r = BitReader::new(&bytes, 0);
        let mut at = 0usize;
        for n in (1..=16u32).cycle().take(80) {
            let want = (0..n as usize).fold(0, |v, k| v * 2 + bit(&data, at + k));
            assert_eq!(r.read_bits(n).unwrap(), want, "{n} bits at bit {at}");
            at += n as usize;
        }
    }

    /// Packs MSB-first `bits` (each `0` or `1`) into bytes, `1`-pads the last byte, byte-stuffs
    /// (§F.1.2.3) and appends EOI: a scan's entropy data as an encoder writes it.
    fn entropy_bytes(bits: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in bits.chunks(8) {
            let byte = (0..8).fold(0u8, |v, i| v * 2 + chunk.get(i).copied().unwrap_or(1));
            out.push(byte);
            if byte == 0xFF {
                out.push(0x00);
            }
        }
        out.extend([0xFF, 0xD9]);
        out
    }

    /// `decode_coded` against `DECODE`, `RECEIVE` and `EXTEND` done separately (`decode_symbol`,
    /// `read_bits`, `extend`): every symbol of the Annex K DC and AC tables, each followed by its
    /// smallest, a middle and its largest magnitude, decodes to the same symbol and value and
    /// leaves the reader at the same bit. Short codes with short magnitudes resolve through the
    /// fused table, longer ones through the lookahead table or the walk, so all three paths run.
    #[test]
    fn decode_coded_matches_decode_then_receive_and_extend() {
        use crate::huffman::{EncTable, STD_CHROMA_AC, STD_LUMA_AC, STD_LUMA_DC};
        for spec in [&STD_LUMA_DC, &STD_LUMA_AC, &STD_CHROMA_AC] {
            let enc = EncTable::from_spec(spec);
            let dec = DecTable::from_bits(&spec.bits, spec.values).unwrap();
            let mut bits = Vec::new();
            let mut want = Vec::new();
            for &symbol in spec.values {
                let size = u32::from(symbol & 0x0F);
                let (code, length) = enc.lookup(symbol).unwrap();
                for magnitude in [0, (1u32 << size) / 3, (1u32 << size) - 1] {
                    bits.extend((0..length).rev().map(|i| (code >> i) as u8 & 1));
                    bits.extend((0..size).rev().map(|i| (magnitude >> i) as u8 & 1));
                    want.push(symbol);
                }
            }
            let bytes = entropy_bytes(&bits);
            let (mut fused, mut split) = (BitReader::new(&bytes, 0), BitReader::new(&bytes, 0));
            for &symbol in &want {
                let size = symbol & 0x0F;
                let (got, value) = decode_coded(&dec, &mut fused).unwrap();
                fused.consume(u32::from(size)).unwrap();
                assert_eq!(decode_symbol(&dec, &mut split).unwrap(), symbol);
                let expected = match size {
                    0 => 0,
                    _ => extend(split.read_bits(u32::from(size)).unwrap(), size),
                };
                assert_eq!((got, value), (symbol, expected), "symbol {symbol:#04x}");
            }
            assert_eq!(fused.read_bits(16).unwrap(), split.read_bits(16).unwrap());
        }
    }

    /// `decode_coded` consumes only the code: a stream that ends inside the magnitude bits decodes
    /// the symbol, and the truncation surfaces at the caller's `consume` of the magnitude -- where
    /// `RECEIVE` after `DECODE` reports it. Luma AC `0x01` is `00` and one magnitude bit; the byte
    /// holds two such codes with their magnitudes (`+1`, `−1`) and then a third code alone.
    #[test]
    fn decode_coded_leaves_a_truncated_magnitude_to_the_caller() {
        let dec = DecTable::from_bits(
            &crate::huffman::STD_LUMA_AC.bits,
            crate::huffman::STD_LUMA_AC.values,
        )
        .unwrap();
        let mut r = BitReader::new(&[0b0010_0000], 0);
        for value in [1, -1] {
            assert_eq!(decode_coded(&dec, &mut r).unwrap(), (0x01, value));
            r.consume(1).unwrap();
        }
        assert_eq!(decode_coded(&dec, &mut r).unwrap().0, 0x01);
        let err = r.consume(1).unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("JPEG: truncated entropy-coded data")
        );
        assert_eq!(err.byte_offset(), Some(1));
    }

    /// `BitReader::fetch` over a stream with fill bytes (§B.1.1.2) both before a stuffed `0xFF
    /// 0x00` and before the closing marker: the fill is skipped, the run's last `0xFF` pairs with
    /// the byte after it, and the cursor parks on the marker's first byte, which
    /// `BitReader::end_marker` then reports.
    #[test]
    fn fetch_skips_fill_bytes_and_parks_on_the_marker() {
        let bytes = [
            0x12, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x34, 0xFF, 0xFF, 0xFF, 0xD9,
        ];
        let mut r = BitReader::new(&bytes, 0);
        let mut data = Vec::new();
        for _ in 0..bytes.len() {
            match r.fetch() {
                Fetch::Data(b) => data.push(b),
                Fetch::Marker | Fetch::Eof => break,
            }
        }
        assert_eq!(data, [0x12, 0xFF, 0xFF, 0x34]);
        assert_eq!((r.marker, r.marker_off), (Some(0xD9), 9));
        assert_eq!(BitReader::new(&bytes, 0).end_marker().unwrap(), (0xD9, 9));
    }

    /// `BitReader::end_marker` on entropy data that ends in fill bytes with no marker after them:
    /// the input runs out first.
    #[test]
    fn end_marker_without_a_marker_is_missing() {
        let err = BitReader::new(&[0x12, 0xFF, 0xFF], 0)
            .end_marker()
            .unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("JPEG: missing marker after scan")
        );
    }

    /// `BitReader::take_restart` skips the rest of the interval's data and the fill before
    /// `RST3`, and resumes on the byte after the marker.
    #[test]
    fn take_restart_resumes_after_the_marker() {
        let mut r = BitReader::new(&[0x56, 0xFF, 0xFF, 0xD3, 0x78], 0);
        r.take_restart(3).unwrap();
        assert!(matches!(r.fetch(), Fetch::Data(0x78)));
    }
}
