//! Most-significant-bit-first bit reader for the AV1 uncompressed headers (AV1 §4.10, §8.1).
//!
//! The mirror of [`BitWriter`](crate::BitWriter): where the writer emits `f(n)` fields into the
//! sequence and frame headers, this reads them back. It covers every descriptor the AV1
//! uncompressed syntax uses — `f(n)`, `su(n)`, `ns(n)`, `uvlc()`, `le(n)`, and byte-aligned
//! `leb128()` — and reports a truncated bitstream as a typed error rather than padding with
//! zeroes. (The *symbol* decoder in [`crate::SymbolDecoder`] does pad, because AV1 §8.2.2
//! explicitly reads past the end of a tile; that is a different parsing process.)
//!
//! Reads are bounds-checked against the backing slice, so a hostile or truncated header cannot
//! index out of range or spin: every method either consumes bits and returns a value, or returns
//! an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error and leaves the reader
//! positioned where it failed.

use gamut_core::{Error, Result};

/// Origin tag on every error this module raises.
const ORIGIN: &str = "gamut-bitstream";

/// AV1 caps `uvlc()` at 32 leading zeroes; beyond that the value is `1 << 32 - 1` (§4.10.3).
const UVLC_MAX_LEADING_ZEROS: u32 = 32;

/// A most-significant-bit-first reader over a byte slice.
///
/// Constructed with [`BitReader::new`], it tracks a bit position within `data`. Fixed-width fields
/// are read with [`BitReader::f`]; the AV1 variable-width descriptors have their own methods. The
/// reader never allocates and borrows its input.
///
/// ```
/// use gamut_bitstream::BitReader;
///
/// // 0b1011_0010 — a 3-bit field then a 5-bit field.
/// let mut r = BitReader::new(&[0b1011_0010]);
/// assert_eq!(r.f(3).unwrap(), 0b101);
/// assert_eq!(r.f(5).unwrap(), 0b1_0010);
/// assert_eq!(r.bits_remaining(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    /// The backing bytes.
    data: &'a [u8],
    /// Read cursor, in bits from the start of `data`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the first bit of `data`.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// The bit position of the read cursor, counted from the start of the input.
    #[must_use]
    pub const fn bit_position(&self) -> usize {
        self.bit_pos
    }

    /// The number of unread bits.
    #[must_use]
    pub const fn bits_remaining(&self) -> usize {
        // `bit_pos` never advances past `data.len() * 8` — every reader method bounds-checks
        // first — so the subtraction cannot wrap.
        self.data.len() * 8 - self.bit_pos
    }

    /// Whether the cursor sits on a byte boundary.
    #[must_use]
    pub const fn is_byte_aligned(&self) -> bool {
        self.bit_pos.is_multiple_of(8)
    }

    /// The bytes not yet consumed, from the next whole byte boundary at or after the cursor.
    ///
    /// Used to hand a trailing payload (an OBU's tile data, say) to another parser once the
    /// header before it has been read and aligned.
    #[must_use]
    pub fn remaining_bytes(&self) -> &'a [u8] {
        let byte = self.bit_pos.div_ceil(8);
        // `bit_pos <= data.len() * 8`, so the rounded-up byte index is at most `data.len()`.
        &self.data[byte.min(self.data.len())..]
    }

    /// Reads the `f(n)` descriptor: `n` bits, most-significant first (AV1 §4.10.2).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer
    /// than `n` bits remain, or if `n > 32`.
    pub fn f(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 f(n): field wider than 32 bits",
            ));
        }
        if (n as usize) > self.bits_remaining() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 f(n): bitstream truncated",
            ));
        }
        let mut x = 0u32;
        for _ in 0..n {
            let byte = self.data[self.bit_pos >> 3];
            let bit = (byte >> (7 - (self.bit_pos & 7))) & 1;
            x = (x << 1) | u32::from(bit);
            self.bit_pos += 1;
        }
        Ok(x)
    }

    /// Reads a 64-bit `f(n)` field, for the few syntax elements wider than 32 bits.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer
    /// than `n` bits remain, or if `n > 64`.
    pub fn f64(&mut self, n: u32) -> Result<u64> {
        if n > 64 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 f(n): field wider than 64 bits",
            ));
        }
        let mut x = 0u64;
        let mut left = n;
        while left > 0 {
            let take = left.min(32);
            x = (x << take) | u64::from(self.f(take)?);
            left -= take;
        }
        Ok(x)
    }

    /// Reads a single bit as a flag.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// bitstream is exhausted.
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.f(1)? != 0)
    }

    /// Reads the `su(n)` descriptor: an `n`-bit two's-complement signed integer (AV1 §4.10.6).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer
    /// than `n` bits remain, or if `n` is 0 or `> 32`.
    pub fn su(&mut self, n: u32) -> Result<i32> {
        if n == 0 {
            return Err(Error::invalid_input(ORIGIN, "AV1 su(n): zero-width field"));
        }
        let value = self.f(n)?;
        let sign_mask = 1u32 << (n - 1);
        Ok(if value & sign_mask != 0 {
            // Sign-extend: subtract 2^n. Done in i64 so `n == 32` cannot overflow.
            (i64::from(value) - (1i64 << n)) as i32
        } else {
            value as i32
        })
    }

    /// Reads the `ns(n)` descriptor: an unsigned value in `0..n`, coded in `FloorLog2(n)` or
    /// `FloorLog2(n) + 1` bits (AV1 §4.10.7).
    ///
    /// Any non-zero `n` is accepted, up to [`u32::MAX`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// bitstream is truncated or `n` is 0.
    pub fn ns(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Err(Error::invalid_input(ORIGIN, "AV1 ns(n): empty range"));
        }
        let w = 32 - n.leading_zeros(); // FloorLog2(n) + 1
        // `w` reaches 32 once `n >= 2^31`, so the spec's `(1 << w) - n` and `(v << 1) - m` are
        // evaluated in u64: in u32 the shift is a full type width, which panics under
        // `debug_assertions` and otherwise masks to `1 << 0`, leaving `m` one too large. Every
        // result is `< n`, so it fits back in u32.
        let m = (1u64 << w) - u64::from(n);
        let v = u64::from(self.f(w - 1)?);
        if v < m {
            return Ok(v as u32);
        }
        let extra = u64::from(self.f(1)?);
        Ok(((v << 1) - m + extra) as u32)
    }

    /// Reads the `uvlc()` descriptor: a variable-length unsigned integer (AV1 §4.10.3).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// bitstream is truncated before the value completes.
    pub fn uvlc(&mut self) -> Result<u32> {
        // §4.10.3 counts leading zeroes until the terminating one bit *first*, and only then
        // tests the 32 threshold. Returning early at the 32nd zero would leave the cursor before
        // that one bit and desync every field after it. The loop is bounded by the input: `f(1)`
        // fails once the bitstream is exhausted.
        let mut leading_zeros = 0u32;
        while self.f(1)? == 0 {
            leading_zeros += 1;
        }
        if leading_zeros >= UVLC_MAX_LEADING_ZEROS {
            // No suffix is coded; the value saturates.
            return Ok(u32::MAX);
        }
        let value = self.f(leading_zeros)?;
        Ok(value + (1u32 << leading_zeros) - 1)
    }

    /// Reads the `le(n)` descriptor: an `n`-byte little-endian unsigned integer (AV1 §4.10.4).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// reader is not byte-aligned, `n > 8`, or fewer than `n` bytes remain.
    pub fn le(&mut self, n: u32) -> Result<u64> {
        if !self.is_byte_aligned() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 le(n): reader is not byte-aligned",
            ));
        }
        if n > 8 {
            return Err(Error::invalid_input(ORIGIN, "AV1 le(n): more than 8 bytes"));
        }
        let mut value = 0u64;
        for i in 0..n {
            value |= u64::from(self.f(8)?) << (i * 8);
        }
        Ok(value)
    }

    /// Reads the byte-aligned `leb128()` descriptor (AV1 §4.10.5), the OBU size encoding.
    ///
    /// Accepts up to the spec's 8 bytes and rejects a ninth continuation byte, a value that
    /// exceeds 32 bits, and a truncated encoding.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// reader is not byte-aligned or the encoding is malformed, over-long, or truncated.
    pub fn leb128(&mut self) -> Result<u64> {
        if !self.is_byte_aligned() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 leb128(): reader is not byte-aligned",
            ));
        }
        let mut value = 0u64;
        for i in 0..8u32 {
            let byte = self.f(8)?;
            value |= u64::from(byte & 0x7f) << (i * 7);
            if byte & 0x80 == 0 {
                if value > u64::from(u32::MAX) {
                    return Err(Error::invalid_input(
                        ORIGIN,
                        "AV1 leb128(): value exceeds 32 bits",
                    ));
                }
                return Ok(value);
            }
        }
        Err(Error::invalid_input(
            ORIGIN,
            "AV1 leb128(): more than 8 bytes",
        ))
    }

    /// Skips `n` bits.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer
    /// than `n` bits remain.
    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if n > self.bits_remaining() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 bit reader: skip past end of bitstream",
            ));
        }
        self.bit_pos += n;
        Ok(())
    }

    /// Advances to the next byte boundary, requiring the skipped bits to be zero
    /// (`byte_alignment()`, AV1 §5.3.5).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// padding bits are not zero or the input is truncated.
    pub fn byte_alignment(&mut self) -> Result<()> {
        while !self.is_byte_aligned() {
            if self.f(1)? != 0 {
                return Err(Error::invalid_input(
                    ORIGIN,
                    "AV1 byte_alignment(): non-zero padding bit",
                ));
            }
        }
        Ok(())
    }

    /// Advances to the next byte boundary without inspecting the skipped bits.
    ///
    /// Use this where the spec aligns without asserting the padding (`trailing_bits` handles the
    /// asserted case).
    pub const fn align(&mut self) {
        self.bit_pos = self.bit_pos.div_ceil(8) * 8;
    }

    /// Reads `trailing_bits()` (AV1 §5.3.4): a `1` bit followed by zeroes to the byte boundary.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// marker bit is absent or a padding bit is set.
    pub fn trailing_bits(&mut self) -> Result<()> {
        if self.f(1)? != 1 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 trailing_bits(): missing marker bit",
            ));
        }
        self.byte_alignment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BitWriter;

    #[test]
    fn f_reads_back_what_the_writer_wrote() {
        let mut w = BitWriter::new();
        w.put_bits(0b101, 3);
        w.put_bits(0xdead_beef, 32);
        w.put_bits(0, 1);
        w.byte_align();
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.f(3).unwrap(), 0b101);
        assert_eq!(r.f(32).unwrap(), 0xdead_beef);
        assert_eq!(r.f(1).unwrap(), 0);
    }

    #[test]
    fn f_rejects_oversized_and_truncated_fields() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(
            r.f(33).unwrap_err().static_message(),
            Some("AV1 f(n): field wider than 32 bits")
        );
        assert_eq!(
            r.f(9).unwrap_err().static_message(),
            Some("AV1 f(n): bitstream truncated")
        );
        // The failed reads left the cursor untouched, so the field is still readable.
        assert_eq!(r.f(8).unwrap(), 0xff);
        assert_eq!(r.bits_remaining(), 0);
    }

    #[test]
    fn f64_spans_more_than_32_bits() {
        let bytes = 0x0123_4567_89ab_cdefu64.to_be_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.f64(64).unwrap(), 0x0123_4567_89ab_cdef);

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.f64(40).unwrap(), 0x0001_2345_6789);

        let mut r = BitReader::new(&bytes);
        assert_eq!(
            r.f64(65).unwrap_err().static_message(),
            Some("AV1 f(n): field wider than 64 bits")
        );
    }

    #[test]
    fn su_sign_extends() {
        // 4-bit fields: 0b0011 = 3, 0b1101 = -3.
        let mut r = BitReader::new(&[0b0011_1101]);
        assert_eq!(r.su(4).unwrap(), 3);
        assert_eq!(r.su(4).unwrap(), -3);

        // The widest case must not overflow the intermediate.
        let mut r = BitReader::new(&[0x80, 0, 0, 0]);
        assert_eq!(r.su(32).unwrap(), i32::MIN);

        let mut r = BitReader::new(&[0xff]);
        assert_eq!(
            r.su(0).unwrap_err().static_message(),
            Some("AV1 su(n): zero-width field")
        );
    }

    /// §4.10.7 transcribed independently of the reader under test.
    ///
    /// `w` reaches 32 for `n >= 2^31`, so the transcription evaluates the spec's `(1 << w) - n`
    /// and `(v << 1) - m` in u64; in u32 the shift is a full type width and the oracle would
    /// carry the very defect it exists to detect.
    fn spec_ns(bits: &[u8], pos: &mut usize, n: u32) -> u32 {
        let w = 32 - n.leading_zeros();
        let m = (1u64 << w) - u64::from(n);
        let mut v = 0u64;
        for _ in 0..w - 1 {
            v = (v << 1) | u64::from(bits[*pos]);
            *pos += 1;
        }
        if v < m {
            return v as u32;
        }
        let extra = u64::from(bits[*pos]);
        *pos += 1;
        ((v << 1) - m + extra) as u32
    }

    /// The `ns()` inverse: the bits a conforming encoder emits for `value` in `0..n`.
    fn encode_ns(value: u32, n: u32) -> Vec<u8> {
        let w = 32 - n.leading_zeros();
        let m = (1u64 << w) - u64::from(n);
        let value = u64::from(value);
        let mut bits = Vec::new();
        if value < m {
            for i in (0..w - 1).rev() {
                bits.push(((value >> i) & 1) as u8);
            }
        } else {
            let coded = value + m;
            for i in (0..w).rev() {
                bits.push(((coded >> i) & 1) as u8);
            }
        }
        bits
    }

    /// Round-trips one `(value, n)` pair through the independent oracle and through the reader,
    /// pinning the consumed width in both. The width assertion is what catches a cursor desync:
    /// a wrong `m` can return the right value while reading one bit too few.
    fn check_ns(value: u32, n: u32) {
        let bits = encode_ns(value, n);
        let mut packed = BitWriter::new();
        for b in &bits {
            packed.put_bit(*b);
        }
        packed.byte_align();
        let bytes = packed.into_bytes();

        let mut pos = 0;
        assert_eq!(
            spec_ns(&bits, &mut pos, n),
            value,
            "spec ns n={n} v={value}"
        );
        assert_eq!(pos, bits.len(), "spec ns consumed the wrong width");

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ns(n).unwrap(), value, "reader ns n={n} v={value}");
        assert_eq!(r.bit_position(), pos, "ns consumed the wrong width");
    }

    #[test]
    fn ns_matches_the_spec_definition() {
        // Every value in 0..n must round-trip for a range that exercises both branches.
        for n in 1u32..=17 {
            for value in 0..n {
                check_ns(value, n);
            }
        }

        let mut r = BitReader::new(&[0xff]);
        assert_eq!(
            r.ns(0).unwrap_err().static_message(),
            Some("AV1 ns(n): empty range")
        );
    }

    #[test]
    fn ns_spans_the_whole_u32_range() {
        // `n >= 2^31` drives `w` to 32, where `(1 << w)` is a full-width u32 shift. Enumerating
        // 0..n is impossible at this scale, so each `n` is probed at the branch boundary `m`,
        // which is where a wrong `m` changes either the value or the bit count.

        // n = 2^31: m == 2^31 and `v` is 31 bits, so every value takes the short branch. The
        // masked-shift `m` (one too large) agrees here, which is why this case pins only that
        // the full-width shift itself is gone.
        for value in [0, 1, (1u32 << 31) - 1] {
            check_ns(value, 1u32 << 31);
        }

        // n = 2^31 + 1: m == 2^31 - 1. The last two values cross into the long branch.
        for value in [0, (1u32 << 31) - 2, (1u32 << 31) - 1, 1u32 << 31] {
            check_ns(value, (1u32 << 31) + 1);
        }

        // n = u32::MAX: m == 1, so every value but 0 takes the long branch. Value 1 is the
        // desync case (an `m` of 2 returns the right value from the short branch, one bit
        // short) and value 2 is the off-by-one case (it returns 1).
        for value in [0, 1, 2, u32::MAX - 1] {
            check_ns(value, u32::MAX);
        }
    }

    #[test]
    fn uvlc_decodes_the_exponential_golomb_form() {
        // 1 -> 0; 010 -> 1; 011 -> 2; 00100 -> 3.
        let mut w = BitWriter::new();
        w.put_bits(0b1, 1);
        w.put_bits(0b010, 3);
        w.put_bits(0b011, 3);
        w.put_bits(0b00100, 5);
        w.byte_align();
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.uvlc().unwrap(), 0);
        assert_eq!(r.uvlc().unwrap(), 1);
        assert_eq!(r.uvlc().unwrap(), 2);
        assert_eq!(r.uvlc().unwrap(), 3);
    }

    #[test]
    fn uvlc_saturates_at_32_leading_zeros() {
        // Exactly 32 zeroes then the terminating one: saturates, and consumes that one bit too.
        let bytes = [0u8, 0, 0, 0, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.uvlc().unwrap(), u32::MAX);
        assert_eq!(r.bit_position(), 33, "the terminating one bit is consumed");
    }

    #[test]
    fn uvlc_consumes_every_leading_zero_past_the_threshold() {
        // 35 zeroes then the terminating one. §4.10.3 counts zeroes to the one bit *before*
        // testing the threshold, so the cursor must land at 36 — an early return at 32 would
        // desync everything that follows.
        let bytes = [0u8, 0, 0, 0, 0b0001_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.uvlc().unwrap(), u32::MAX);
        assert_eq!(r.bit_position(), 36);
    }

    #[test]
    fn uvlc_at_31_leading_zeros_still_reads_its_suffix() {
        // The largest non-saturating case: 31 zeroes, the one bit, then a 31-bit suffix.
        let mut w = BitWriter::new();
        w.put_bits(0, 31);
        w.put_bits(1, 1);
        w.put_bits(0, 31);
        w.byte_align();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.uvlc().unwrap(), (1u32 << 31) - 1);
        assert_eq!(r.bit_position(), 63);
    }

    #[test]
    fn uvlc_reports_truncation_rather_than_looping() {
        // All zeroes and no terminating one bit: the loop must end on the exhausted input.
        let mut r = BitReader::new(&[0b0000_0000]);
        assert_eq!(
            r.uvlc().unwrap_err().static_message(),
            Some("AV1 f(n): bitstream truncated")
        );

        // A complete prefix whose suffix is cut short is also refused: four leading zeroes and
        // the marker leave only three bits for a four-bit suffix.
        let mut r = BitReader::new(&[0b0000_1000]);
        assert_eq!(
            r.uvlc().unwrap_err().static_message(),
            Some("AV1 f(n): bitstream truncated")
        );
    }

    #[test]
    fn le_reads_little_endian_bytes() {
        let mut r = BitReader::new(&[0x34, 0x12]);
        assert_eq!(r.le(2).unwrap(), 0x1234);

        // Eight bytes is the widest field `le` accepts, so it is the exact boundary the `n > 8`
        // guard decides; a guard mutated to `>=` rejects this.
        let mut r = BitReader::new(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(r.le(8).unwrap(), 0xefcd_ab89_6745_2301);

        let mut r = BitReader::new(&[0xff; 9]);
        assert_eq!(
            r.le(9).unwrap_err().static_message(),
            Some("AV1 le(n): more than 8 bytes")
        );

        let mut r = BitReader::new(&[0xff; 4]);
        r.f(1).unwrap();
        assert_eq!(
            r.le(1).unwrap_err().static_message(),
            Some("AV1 le(n): reader is not byte-aligned")
        );
    }

    #[test]
    fn leb128_round_trips_against_the_writer() {
        for &v in &[0u64, 1, 127, 128, 300, 0xffff, 0x10_0000, u32::MAX as u64] {
            let mut out = Vec::new();
            crate::write_leb128(&mut out, v);
            let mut r = BitReader::new(&out);
            assert_eq!(r.leb128().unwrap(), v, "leb128 round-trip for {v}");
            assert_eq!(r.bits_remaining(), 0);
        }
    }

    #[test]
    fn leb128_rejects_overlong_oversized_and_misaligned() {
        // Nine continuation bytes.
        let mut r = BitReader::new(&[0x80; 9]);
        assert_eq!(
            r.leb128().unwrap_err().static_message(),
            Some("AV1 leb128(): more than 8 bytes")
        );

        // A value above u32::MAX is refused rather than silently truncated.
        let mut out = Vec::new();
        crate::write_leb128(&mut out, u64::from(u32::MAX) + 1);
        let mut r = BitReader::new(&out);
        assert_eq!(
            r.leb128().unwrap_err().static_message(),
            Some("AV1 leb128(): value exceeds 32 bits")
        );

        let mut r = BitReader::new(&[0x00, 0x00]);
        r.f(1).unwrap();
        assert_eq!(
            r.leb128().unwrap_err().static_message(),
            Some("AV1 leb128(): reader is not byte-aligned")
        );
    }

    #[test]
    fn trailing_bits_accepts_the_marker_and_rejects_padding() {
        // Three data bits, then the marker 1, then zeroes to the boundary.
        let mut r = BitReader::new(&[0b0001_0000]);
        r.f(3).unwrap();
        r.trailing_bits().unwrap();
        assert_eq!(r.bits_remaining(), 0);

        // A set padding bit is a malformed stream.
        let mut r = BitReader::new(&[0b1000_0001]);
        assert_eq!(
            r.trailing_bits().unwrap_err().static_message(),
            Some("AV1 byte_alignment(): non-zero padding bit")
        );

        // No marker bit at all.
        let mut r = BitReader::new(&[0b0000_0000]);
        assert_eq!(
            r.trailing_bits().unwrap_err().static_message(),
            Some("AV1 trailing_bits(): missing marker bit")
        );
    }

    #[test]
    fn alignment_helpers_track_the_cursor() {
        let mut r = BitReader::new(&[0xff, 0x0f, 0xaa]);
        assert!(r.is_byte_aligned());
        r.f(3).unwrap();
        assert!(!r.is_byte_aligned());
        assert_eq!(r.remaining_bytes(), &[0x0f, 0xaa]);
        r.align();
        assert_eq!(r.bit_position(), 8);
        r.align();
        assert_eq!(r.bit_position(), 8, "align on a boundary is a no-op");

        assert_eq!(
            r.skip_bits(17).unwrap_err().static_message(),
            Some("AV1 bit reader: skip past end of bitstream")
        );
        r.skip_bits(16).unwrap();
        assert_eq!(r.bits_remaining(), 0);
        assert_eq!(r.remaining_bytes(), &[] as &[u8]);
    }

    #[test]
    fn flag_reads_single_bits() {
        let mut r = BitReader::new(&[0b1010_0000]);
        assert!(r.flag().unwrap());
        assert!(!r.flag().unwrap());
        assert!(r.flag().unwrap());
        assert!(!r.flag().unwrap());
    }
}
