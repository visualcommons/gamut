//! Most-significant-bit-first bit writer for AV1 uncompressed headers (AV1 §4, §8.1).

/// A most-significant-bit-first bit writer.
///
/// AV1 uncompressed headers and OBU framing are described with `f(n)` fixed-width fields that a
/// decoder reads most-significant-bit first (AV1 §8.1, "Parsing process for `f(n)`"); this writer
/// is the encoder-side mirror. Bits accumulate into an internal byte buffer; the buffer is byte
/// aligned exactly when [`BitWriter::is_byte_aligned`] returns `true`.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits already filled in the current (last) partial byte, `0..=7`. `0` means byte aligned.
    bit_pos: u8,
}

impl BitWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes a single bit (`f(1)`); only the low bit of `bit` is used.
    pub fn put_bit(&mut self, bit: u8) {
        if self.bit_pos == 0 {
            self.bytes.push(0);
        }
        if bit & 1 != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.bit_pos);
        }
        self.bit_pos = (self.bit_pos + 1) & 7;
    }

    /// Writes the low `n` bits of `value`, most-significant bit first (`f(n)`). `n` must be `0..=32`.
    pub fn put_bits(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            self.put_bit(((value >> i) & 1) as u8);
        }
    }

    /// Number of bits written so far.
    #[must_use]
    pub fn bit_len(&self) -> usize {
        if self.bit_pos == 0 {
            self.bytes.len() * 8
        } else {
            (self.bytes.len() - 1) * 8 + self.bit_pos as usize
        }
    }

    /// Returns `true` when the next bit would start a fresh byte.
    #[must_use]
    pub fn is_byte_aligned(&self) -> bool {
        self.bit_pos == 0
    }

    /// Pads with zero bits up to the next byte boundary (a no-op when already aligned): the mirror
    /// of [`BitReader::align`](crate::BitReader::align), which requires those bits to be zero.
    /// This is AV1's `byte_alignment()` (§5.3.5).
    pub fn align(&mut self) {
        while self.bit_pos != 0 {
            self.put_bit(0);
        }
    }

    /// Appends whole bytes verbatim. Requires the writer to be byte aligned.
    ///
    /// # Panics
    ///
    /// Debug builds assert byte alignment; misuse in release silently misaligns the stream.
    pub fn put_bytes(&mut self, data: &[u8]) {
        debug_assert!(self.is_byte_aligned(), "put_bytes requires byte alignment");
        self.bytes.extend_from_slice(data);
    }

    /// Appends the minimal unsigned LEB128 encoding of `value` (AV1 §4.10.5): the mirror of
    /// [`BitReader::leb128`](crate::BitReader::leb128). Requires the writer to be byte aligned,
    /// as the reader does. The encoding is [`write_leb128`](crate::write_leb128)'s, which serves
    /// a caller building a plain byte buffer.
    ///
    /// # Panics
    ///
    /// Debug builds assert byte alignment; misuse in release silently misaligns the stream.
    pub fn put_leb128(&mut self, value: u64) {
        debug_assert!(self.is_byte_aligned(), "put_leb128 requires byte alignment");
        crate::write_leb128(&mut self.bytes, value);
    }

    /// Borrows the bytes written so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the writer, returning the byte buffer (trailing bits in the final partial byte are
    /// zero-filled).
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `n` bits MSB-first starting at `bit_pos`; mirrors the AV1 `f(n)` decoder so tests can
    /// assert the exact bit layout a real decoder would observe.
    fn read_bits(bytes: &[u8], bit_pos: &mut usize, n: u32) -> u32 {
        let mut x = 0u32;
        for _ in 0..n {
            let byte = bytes[*bit_pos >> 3];
            let bit = (byte >> (7 - (*bit_pos & 7))) & 1;
            x = (x << 1) | u32::from(bit);
            *bit_pos += 1;
        }
        x
    }

    #[test]
    fn put_bits_is_msb_first() {
        let mut w = BitWriter::new();
        w.put_bits(0b101, 3);
        w.put_bits(0b01, 2);
        assert_eq!(w.bit_len(), 5);
        // 0b101_01 packed MSB-first into one byte: 1010_1000 = 0xA8.
        assert_eq!(w.as_bytes(), &[0xA8]);
    }

    #[test]
    fn align_pads_with_zeros() {
        let mut w = BitWriter::new();
        w.put_bit(1);
        w.align();
        assert!(w.is_byte_aligned());
        assert_eq!(w.into_bytes(), &[0x80]);
    }

    #[test]
    fn put_bytes_after_alignment() {
        let mut w = BitWriter::new();
        w.put_bits(0xF, 4);
        w.align();
        w.put_bytes(&[0x12, 0x34]);
        assert_eq!(w.into_bytes(), &[0xF0, 0x12, 0x34]);
    }

    #[test]
    fn put_leb128_appends_the_minimal_encoding() {
        let mut w = BitWriter::new();
        w.put_bits(0xF, 4);
        w.align();
        w.put_leb128(300);
        assert_eq!(w.into_bytes(), &[0xF0, 0xac, 0x02]);
    }

    #[test]
    fn roundtrip_various_widths() {
        let mut w = BitWriter::new();
        let fields = [(0x1, 1u32), (0x2A, 6), (0xFFFF, 16), (0x0, 3), (0x7, 3)];
        for &(v, n) in &fields {
            w.put_bits(v, n);
        }
        let bytes = w.into_bytes();
        let mut pos = 0usize;
        for &(v, n) in &fields {
            assert_eq!(read_bits(&bytes, &mut pos, n), v, "field {v:#x}/{n}");
        }
    }

    #[test]
    fn bit_len_and_alignment_track_writes() {
        let mut w = BitWriter::new();
        assert_eq!(w.bit_len(), 0);
        assert!(w.is_byte_aligned());

        // Aligned with one full byte buffered: bit_len is `bytes.len() * 8`. Distinguishes the
        // multiply from `+`/`/` (1 + 8 = 9, 1 / 8 = 0, both != 8).
        w.put_bits(0xAB, 8);
        assert_eq!(w.bit_len(), 8);
        assert!(w.is_byte_aligned());

        // Two full bytes: pins `bytes.len() * 8` again with a different length (2 * 8 = 16).
        w.put_bits(0xCD, 8);
        assert_eq!(w.bit_len(), 16);

        // Partial byte with more than one byte buffered: bit_len is `(bytes.len() - 1) * 8 +
        // bit_pos`. With bytes.len() == 3 the `* 8` is distinguishable from `/ 8`
        // ((3 - 1) * 8 + 5 = 21 vs (3 - 1) / 8 + 5 = 5), and the writer is no longer aligned, so
        // `is_byte_aligned` cannot be a constant `true`.
        w.put_bits(0b10101, 5);
        assert_eq!(w.bit_len(), 21);
        assert!(!w.is_byte_aligned());
    }
}
