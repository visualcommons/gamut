//! Least-significant-bit-first bit reader, the bit order of VP8L (RFC 9649 §3.3) and DEFLATE
//! (RFC 1951 §3.1.1).
//!
//! The counterpart of [`BitReader`](crate::BitReader) for the formats that pack fields from the low
//! bit of each byte upward: bytes are read in stream order and the bits of each byte
//! least-significant first, and a multi-bit field is assembled in that same order, so
//! `read_bits(2)` equals `read_bit() | (read_bit() << 1)` (RFC 9649 §3.3, `ReadBits(n)`).
//!
//! Reads are bounds-checked against the backing slice: running past the end of the input is an
//! [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error carrying the byte offset
//! where the input ran out, never a panic and never zero padding.

use gamut_core::{Error, Result};

/// Origin tag on every error this module raises.
const ORIGIN: &str = "gamut-bitstream";

/// The widest single field one read returns.
const MAX_BITS_PER_READ: u32 = 32;

/// A least-significant-bit-first reader over a byte slice.
///
/// Constructed with [`LsbBitReader::new`], it hands out fields with [`LsbBitReader::read_bits`]
/// (RFC 9649's `ReadBits(n)`) and single bits with [`LsbBitReader::read_bit`]. Bytes are pulled
/// one at a time into a small accumulator, so a read touches only the bytes it needs. The reader
/// never allocates and borrows its input.
///
/// ```
/// use gamut_bitstream::LsbBitReader;
///
/// // 0b1011_0010, low bit first: a 1-bit field (0), a 3-bit field (0b001), a 4-bit field (0b1011).
/// let mut r = LsbBitReader::new(&[0b1011_0010]);
/// assert_eq!(r.read_bit().unwrap(), 0);
/// assert_eq!(r.read_bits(3).unwrap(), 0b001);
/// assert_eq!(r.read_bits(4).unwrap(), 0b1011);
/// assert_eq!(r.bits_remaining(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct LsbBitReader<'a> {
    /// The backing bytes.
    data: &'a [u8],
    /// Index of the next not-yet-buffered byte in `data`.
    byte_pos: usize,
    /// Buffered bits, right-aligned: the next bit to return is bit 0.
    acc: u64,
    /// Number of valid low bits in `acc` (always `< 8` between reads).
    bits_in_acc: u32,
}

impl<'a> LsbBitReader<'a> {
    /// Creates a reader positioned at the first bit of `data`.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            acc: 0,
            bits_in_acc: 0,
        }
    }

    /// The bit position of the read cursor, counted from the start of the input.
    #[must_use]
    pub const fn bit_position(&self) -> usize {
        self.byte_pos * 8 - self.bits_in_acc as usize
    }

    /// The number of unread bits.
    #[must_use]
    pub const fn bits_remaining(&self) -> usize {
        // The cursor never passes `data.len() * 8`: a byte is buffered only when it exists.
        self.data.len() * 8 - self.bit_position()
    }

    /// Reads `n` bits (`0..=32`) least-significant first, returning them right-aligned.
    ///
    /// Reading 0 bits returns `Ok(0)` without consuming input (VP8L's single-symbol prefix codes
    /// code no bits). A failed read leaves the cursor where it was.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer
    /// than `n` bits remain, or if `n > 32`; either carries the byte offset of the failure.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n > MAX_BITS_PER_READ {
            return Err(Error::invalid_input(ORIGIN, "bit field wider than 32 bits")
                .with_byte_offset((self.bit_position() / 8) as u64));
        }
        // Bounded by the input it walks: every pass buffers one more byte or returns.
        while self.bits_in_acc < n {
            let Some(&byte) = self.data.get(self.byte_pos) else {
                return Err(Error::invalid_input(ORIGIN, "bitstream truncated")
                    .with_byte_offset(self.byte_pos as u64));
            };
            self.acc |= u64::from(byte) << self.bits_in_acc;
            self.bits_in_acc += 8;
            self.byte_pos += 1;
        }
        let value = (self.acc & ((1u64 << n) - 1)) as u32;
        self.acc >>= n;
        self.bits_in_acc -= n;
        Ok(value)
    }

    /// Reads a single bit (`0` or `1`).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// input is exhausted.
    pub fn read_bit(&mut self) -> Result<u32> {
        self.read_bits(1)
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn reads_lsb_first_within_a_byte() {
        // 0xB2 = 0b1011_0010: low bit first the bits are 0,1,0,0,1,1,0,1.
        let mut r = LsbBitReader::new(&[0xB2]);
        assert_eq!(r.read_bits(1).unwrap(), 0);
        assert_eq!(r.read_bits(1).unwrap(), 1);
        assert_eq!(r.read_bits(2).unwrap(), 0b00);
        assert_eq!(r.read_bits(4).unwrap(), 0b1011);
    }

    #[test]
    fn multi_bit_read_equals_single_bit_reads() {
        // RFC 9649 §3.3: ReadBits(2) == ReadBits(1) | (ReadBits(1) << 1).
        let data = [0x9D, 0x01, 0x2A, 0xFF];
        let mut a = LsbBitReader::new(&data);
        let mut b = LsbBitReader::new(&data);
        for _ in 0..10 {
            let combined = a.read_bits(2).unwrap();
            let lo = b.read_bit().unwrap();
            let hi = b.read_bit().unwrap();
            assert_eq!(combined, lo | (hi << 1));
        }
    }

    #[test]
    fn fields_cross_byte_boundaries() {
        // The VP8L header layout: the 0x2f signature, then 14-bit width-1 = 16383 and 14-bit
        // height-1 = 0, packed low bit first into 0x2f 0xff 0x3f 0x00 0x00.
        let mut r = LsbBitReader::new(&[0x2f, 0xff, 0x3f, 0x00, 0x00]);
        assert_eq!(r.read_bits(8).unwrap(), 0x2f);
        assert_eq!(r.read_bits(14).unwrap(), 16383);
        assert_eq!(r.read_bits(14).unwrap(), 0);
        assert_eq!(r.bit_position(), 36);
        assert_eq!(r.bits_remaining(), 4);
    }

    #[test]
    fn a_full_32_bit_field_reads_after_an_unaligned_one() {
        // `(0xDEAD_BEEF << 3) | 0b101` in little-endian order: three bits in, the 32-bit read
        // spans five bytes and needs 35 buffered bits.
        let mut r = LsbBitReader::new(&[0x7d, 0xf7, 0x6d, 0xf5, 0x06]);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
        assert_eq!(r.read_bits(32).unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.bits_remaining(), 5);
    }

    #[test]
    fn read_zero_bits_is_a_no_op() {
        let mut r = LsbBitReader::new(&[0xFF]);
        assert_eq!(r.read_bits(0).unwrap(), 0);
        assert_eq!(r.bit_position(), 0);
        assert_eq!(r.read_bits(8).unwrap(), 0xFF);
        // Also at the very end of the input.
        assert_eq!(r.read_bits(0).unwrap(), 0);
    }

    #[test]
    fn running_out_is_invalid_input_at_the_exhausted_byte() {
        let mut r = LsbBitReader::new(&[0xAB, 0x01]);
        assert_eq!(r.read_bits(12).unwrap(), 0x1AB);
        let error = r.read_bits(5).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("bitstream truncated"));
        assert_eq!(error.byte_offset(), Some(2));
        // The failed read moved nothing: the four bits left still read.
        assert_eq!(r.bit_position(), 12);
        assert_eq!(r.read_bits(4).unwrap(), 0);

        let error = LsbBitReader::new(&[]).read_bit().unwrap_err();
        assert_eq!(error.byte_offset(), Some(0));
    }

    #[test]
    fn a_field_wider_than_32_bits_is_rejected_at_the_cursor() {
        let mut r = LsbBitReader::new(&[0; 8]);
        r.read_bits(12).unwrap();
        let error = r.read_bits(33).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("bit field wider than 32 bits"));
        assert_eq!(error.byte_offset(), Some(1));
        assert_eq!(r.bit_position(), 12);
    }

    #[test]
    fn position_and_remaining_track_the_cursor() {
        let mut r = LsbBitReader::new(&[0xFF, 0x0F]);
        assert_eq!(r.bits_remaining(), 16);
        r.read_bits(12).unwrap();
        assert_eq!(r.bit_position(), 12);
        assert_eq!(r.bits_remaining(), 4);
        r.read_bits(4).unwrap();
        assert_eq!(r.bit_position(), 16);
        assert_eq!(r.bits_remaining(), 0);
    }
}
