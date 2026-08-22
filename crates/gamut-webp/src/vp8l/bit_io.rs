//! VP8L bit I/O: an **LSB-first** bit stream (RFC 9649 §3.3).
//!
//! VP8L reads values least-significant-bit-first within each byte — the opposite order to the
//! MSB-first `f(n)` fields used by the AV1-family codecs. That incompatibility is why this LSB-first
//! reader ([`BitReader`], `ReadBits(n)`) and writer ([`BitWriter`]) are implemented in-crate rather
//! than reusing a shared bit-stream primitive. Tracked in `../STATUS.md` section F.
//!
//! Per the spec, "the bytes are read in the natural order of the stream ... and bits of each byte
//! are read in least-significant-bit-first order. When multiple bits are read at the same time, the
//! integer is constructed from the original data in the original order." So `read_bits(2)` is
//! equivalent to `read_bits(1) | (read_bits(1) << 1)`.

use gamut_core::{Error, Result};

/// The widest single field the format reads or writes at once (length/distance extra bits top out
/// at 18, dimensions at 14); reads/writes beyond this are a caller bug.
const MAX_BITS_PER_OP: u32 = 32;

/// An LSB-first bit reader over a VP8L bitstream (RFC 9649 §3.3).
///
/// Bits are pulled a byte at a time into a small accumulator and handed out least-significant-bit
/// first. Running past the end of the input is reported as [`Error::InvalidInput`] rather than
/// panicking, so a truncated stream fails cleanly.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    /// The bytes backing the stream (the VP8L chunk payload, signature byte included).
    data: &'a [u8],
    /// Index of the next not-yet-buffered byte in `data`.
    byte_pos: usize,
    /// Buffered bits, right-aligned: the next bit to return is bit 0.
    acc: u64,
    /// Number of valid low bits currently in `acc` (always `< 8` between operations).
    bits_in_acc: u32,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the start of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            acc: 0,
            bits_in_acc: 0,
        }
    }

    /// Reads `n` bits (`0..=32`) LSB-first, returning them right-aligned in a `u32`.
    ///
    /// Reading 0 bits returns `Ok(0)` without consuming input (used by single-symbol prefix codes
    /// that encode no bits).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if fewer than `n` bits remain, or if `n > 32`.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > MAX_BITS_PER_OP {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: bit read width out of range",
            )
            .with_byte_offset((self.bits_consumed_inner() / 8) as u64));
        }
        while self.bits_in_acc < n {
            let Some(&byte) = self.data.get(self.byte_pos) else {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: unexpected end of bitstream",
                )
                .with_byte_offset(self.byte_pos as u64));
            };
            self.acc |= u64::from(byte) << self.bits_in_acc;
            self.bits_in_acc += 8;
            self.byte_pos += 1;
        }
        let mask = (1u64 << n) - 1;
        let value = (self.acc & mask) as u32;
        self.acc >>= n;
        self.bits_in_acc -= n;
        Ok(value)
    }

    /// Reads a single bit (a convenience wrapper over [`read_bits`](Self::read_bits)).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if no bits remain.
    pub fn read_bit(&mut self) -> Result<u32> {
        self.read_bits(1)
    }

    /// Total number of bits consumed so far.
    #[must_use]
    #[cfg(test)]
    pub fn bits_consumed(&self) -> usize {
        self.bits_consumed_inner()
    }

    fn bits_consumed_inner(&self) -> usize {
        self.byte_pos * 8 - self.bits_in_acc as usize
    }

    /// Whether every bit of the input has been consumed (the trailing partial byte, if any, is
    /// treated as zero padding and is not counted as remaining data).
    #[must_use]
    #[cfg(test)]
    pub fn is_exhausted(&self) -> bool {
        self.byte_pos >= self.data.len() && self.bits_in_acc == 0
    }
}

/// An LSB-first bit writer (the encoder side of [`BitReader`]).
///
/// Bits accumulate low-to-high and are flushed a byte at a time; [`finish`](Self::finish)
/// zero-pads the final partial byte. The writer is infallible (it grows a `Vec`).
#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    /// Completed bytes.
    buf: Vec<u8>,
    /// Pending bits, right-aligned.
    acc: u64,
    /// Number of valid low bits in `acc` (always `< 8` between operations).
    bits_in_acc: u32,
}

impl BitWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the low `n` bits (`0..=32`) of `value`, LSB-first. Bits of `value` above bit `n-1`
    /// are ignored. `n > 32` or `n == 0` writes nothing.
    pub fn write_bits(&mut self, value: u32, n: u32) {
        if n == 0 || n > MAX_BITS_PER_OP {
            return;
        }
        let mask = (1u64 << n) - 1;
        self.acc |= (u64::from(value) & mask) << self.bits_in_acc;
        self.bits_in_acc += n;
        while self.bits_in_acc >= 8 {
            self.buf.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.bits_in_acc -= 8;
        }
    }

    /// Number of bits written so far (including any pending partial byte).
    ///
    /// The measurement hook the encoder's keep-the-smallest choices compare on: a candidate is
    /// written into a scratch writer and scored by this, in bits rather than bytes, because the
    /// stream is byte-padded only once at the very end.
    #[must_use]
    pub fn bit_len(&self) -> usize {
        self.buf.len() * 8 + self.bits_in_acc as usize
    }

    /// Flushes any partial trailing byte (zero-padding its high bits) and returns the bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        if self.bits_in_acc > 0 {
            self.buf.push((self.acc & 0xff) as u8);
        }
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_does_not_append_a_byte_when_byte_aligned() {
        // After writing a whole number of bytes, bits_in_acc == 0, so finish must NOT push a trailing
        // (zero) byte. `bits_in_acc > 0` widened to `>= 0` would append a spurious 0x00.
        let mut w = BitWriter::new();
        w.write_bits(0xAB, 8); // exactly one byte -> bits_in_acc returns to 0
        assert_eq!(w.finish(), vec![0xAB]);
    }

    #[test]
    fn reads_lsb_first_within_a_byte() {
        // 0b1011_0010 = 0xB2. LSB-first the bits are 0,1,0,0,1,1,0,1.
        let mut r = BitReader::new(&[0xB2]);
        assert_eq!(r.read_bits(1).unwrap(), 0);
        assert_eq!(r.read_bits(1).unwrap(), 1);
        assert_eq!(r.read_bits(2).unwrap(), 0b00); // next two bits: 0,0
        assert_eq!(r.read_bits(4).unwrap(), 0b1011); // remaining: 1,1,0,1 -> 0b1011
    }

    #[test]
    fn multi_bit_read_equals_single_bit_reads() {
        // Spec equivalence: read_bits(2) == read_bits(1) | (read_bits(1) << 1).
        let data = [0x9D, 0x01, 0x2A, 0xFF];
        let mut a = BitReader::new(&data);
        let mut b = BitReader::new(&data);
        for _ in 0..10 {
            let combined = a.read_bits(2).unwrap();
            let lo = b.read_bits(1).unwrap();
            let hi = b.read_bits(1).unwrap();
            assert_eq!(combined, lo | (hi << 1));
        }
    }

    #[test]
    fn read_zero_bits_is_noop() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read_bits(0).unwrap(), 0);
        assert_eq!(r.bits_consumed(), 0);
        assert_eq!(r.read_bits(8).unwrap(), 0xFF);
    }

    #[test]
    fn crosses_byte_boundaries() {
        // 0x2f signature byte then a 14-bit width-1 field, exactly as the VP8L header is laid out.
        let mut w = BitWriter::new();
        w.write_bits(0x2f, 8);
        w.write_bits(16383, 14); // width - 1 (max)
        w.write_bits(0, 14); // height - 1
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(8).unwrap(), 0x2f);
        assert_eq!(r.read_bits(14).unwrap(), 16383);
        assert_eq!(r.read_bits(14).unwrap(), 0);
    }

    #[test]
    fn out_of_data_is_invalid_input_not_panic() {
        let mut r = BitReader::new(&[0xAB]);
        assert_eq!(r.read_bits(8).unwrap(), 0xAB);
        assert!(matches!(
            r.read_bits(1),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // Reading from empty input also fails cleanly.
        let mut empty = BitReader::new(&[]);
        assert!(matches!(
            empty.read_bits(1),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn oversized_read_is_rejected() {
        let mut r = BitReader::new(&[0; 8]);
        r.read_bits(12).unwrap();
        let error = r.read_bits(33).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(error.byte_offset(), Some(1));
    }

    #[test]
    fn writer_round_trips_varied_widths() {
        let fields: &[(u32, u32)] = &[
            (0, 1),
            (1, 1),
            (0b101, 3),
            (0, 0),
            (0x3FFF, 14),
            (0xABCD, 16),
            (0x00FF_FF00, 24),
            (0xDEAD_BEEF, 32),
            (7, 3),
        ];
        let mut w = BitWriter::new();
        for &(v, n) in fields {
            w.write_bits(v, n);
        }
        let total_bits: usize = fields.iter().map(|&(_, n)| n as usize).sum();
        assert_eq!(w.bit_len(), total_bits);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for &(v, n) in fields {
            let masked = if n == 0 {
                0
            } else {
                v & ((1u64 << n) - 1) as u32
            };
            assert_eq!(r.read_bits(n).unwrap(), masked, "field {v:#x}/{n}");
        }
    }

    #[test]
    fn partial_final_byte_is_zero_padded() {
        let mut w = BitWriter::new();
        w.write_bits(0b1, 1); // a single set bit, then 7 pad bits
        let bytes = w.finish();
        assert_eq!(bytes, vec![0b0000_0001]);
    }

    #[test]
    fn consumed_and_exhausted_track_position() {
        let mut r = BitReader::new(&[0xFF, 0x0F]);
        assert!(!r.is_exhausted());
        r.read_bits(12).unwrap();
        assert_eq!(r.bits_consumed(), 12);
        assert!(!r.is_exhausted());
        r.read_bits(4).unwrap();
        assert_eq!(r.bits_consumed(), 16);
        assert!(r.is_exhausted());
    }
}
