//! VP8L bit I/O: an **LSB-first** bit stream (RFC 9649 §3.3).
//!
//! VP8L reads values least-significant-bit-first within each byte — the opposite order to the
//! MSB-first `f(n)` fields used by the AV1-family codecs. The reader (`ReadBits(n)`) is the shared
//! [`gamut_bitstream::LsbBitReader`], re-exported here as [`BitReader`]; the writer
//! ([`BitWriter`]), whose only user is this crate's encoder, stays in-crate. Tracked in
//! `../STATUS.md` section F.
//!
//! Per the spec, "the bytes are read in the natural order of the stream ... and bits of each byte
//! are read in least-significant-bit-first order. When multiple bits are read at the same time, the
//! integer is constructed from the original data in the original order." So `read_bits(2)` is
//! equivalent to `read_bits(1) | (read_bits(1) << 1)`.

/// The VP8L reader: the shared LSB-first [`gamut_bitstream::LsbBitReader`].
pub use gamut_bitstream::LsbBitReader as BitReader;

/// The widest single field the format writes at once (length/distance extra bits top out at 18,
/// dimensions at 14); writes beyond this are a caller bug.
const MAX_BITS_PER_OP: u32 = 32;

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
    fn crosses_byte_boundaries() {
        // 0x2f signature byte then a 14-bit width-1 field, exactly as the VP8L header is laid out.
        let mut w = BitWriter::new();
        w.write_bits(0x2f, 8);
        w.write_bits(16383, 14); // width - 1 (max)
        w.write_bits(0, 14); // height - 1
        let bytes = w.finish();
        // The same bytes `gamut_bitstream::LsbBitReader`'s own header fixture reads back.
        assert_eq!(bytes, [0x2f, 0xff, 0x3f, 0x00, 0x00]);
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(8).unwrap(), 0x2f);
        assert_eq!(r.read_bits(14).unwrap(), 16383);
        assert_eq!(r.read_bits(14).unwrap(), 0);
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
}
