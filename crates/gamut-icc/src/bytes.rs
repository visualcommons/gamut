//! A minimal big-endian byte cursor for ICC parsing.
//!
//! ICC data is always big-endian and byte-aligned, so this needs neither a byte-order parameter
//! (unlike `gamut-ifd`'s TIFF readers) nor the bit-level packing of `gamut-bitstream`. Every read is
//! bounds-checked and returns [`IccError::Malformed`] on overrun, keeping the parser panic-free
//! under `#![forbid(unsafe_code)]`. The reader does not locate its own errors: a cursor over a tag
//! element (or a description embedded in one) knows only its slice-relative position, so the
//! profile parser locates every error at the structure it was reading instead.

use crate::error::{IccError, Result};
use crate::primitives::{DateTime, S15Fixed16, Signature, U16Fixed16, XyzNumber};

/// A forward-only big-endian reader over an ICC byte buffer.
pub(crate) struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// A reader positioned at the start of `buf`.
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// A reader positioned at absolute byte `offset` into `buf` (for offset-indexed tag elements).
    pub(crate) fn at(buf: &'a [u8], offset: usize) -> Result<Self> {
        if offset > buf.len() {
            return Err(IccError::Malformed("icc: offset past end of profile"));
        }
        Ok(Self { buf, pos: offset })
    }

    /// The number of unread bytes remaining.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The current absolute byte offset into the buffer (for slicing variable-length sub-elements).
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// Advances the cursor to the next 4-byte boundary, the alignment ICC uses between the curve
    /// sub-elements packed inside a LUT transform.
    pub(crate) fn align_to_4(&mut self) -> Result<()> {
        self.skip(self.pos.next_multiple_of(4) - self.pos)
    }

    /// Reads the next `n` bytes, advancing the cursor; errors if fewer than `n` remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(IccError::Malformed("icc: length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(IccError::Malformed("icc: unexpected end of data"))?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Reads `n` raw bytes.
    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    /// Reads one byte.
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a big-endian `u16`.
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    /// Reads a big-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// Reads a big-endian `u64`.
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// Reads a big-endian `i32`.
    pub(crate) fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    /// Reads a four-byte [`Signature`].
    pub(crate) fn signature(&mut self) -> Result<Signature> {
        Ok(Signature(self.array()?))
    }

    /// Reads an `s15Fixed16Number`.
    pub(crate) fn s15fixed16(&mut self) -> Result<S15Fixed16> {
        Ok(S15Fixed16(self.i32()?))
    }

    /// Reads a `u16Fixed16Number`.
    pub(crate) fn u16fixed16(&mut self) -> Result<U16Fixed16> {
        Ok(U16Fixed16(self.u32()?))
    }

    /// Reads an `XYZNumber` (three consecutive `s15Fixed16`).
    pub(crate) fn xyz_number(&mut self) -> Result<XyzNumber> {
        Ok(XyzNumber {
            x: self.s15fixed16()?,
            y: self.s15fixed16()?,
            z: self.s15fixed16()?,
        })
    }

    /// Reads a `dateTimeNumber` (six consecutive `u16`).
    pub(crate) fn date_time(&mut self) -> Result<DateTime> {
        Ok(DateTime {
            year: self.u16()?,
            month: self.u16()?,
            day: self.u16()?,
            hours: self.u16()?,
            minutes: self.u16()?,
            seconds: self.u16()?,
        })
    }

    /// Advances the cursor by `n` bytes, erroring if fewer remain.
    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

/// Zero-pads `out` up to the next 4-byte boundary (the alignment ICC uses between elements).
pub(crate) fn pad_to_4(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(4), 0);
}

/// Appends an `s15Fixed16` big-endian.
pub(crate) fn push_s15fixed16(out: &mut Vec<u8>, value: S15Fixed16) {
    out.extend_from_slice(&value.0.to_be_bytes());
}

/// Appends a `u16Fixed16` big-endian.
pub(crate) fn push_u16fixed16(out: &mut Vec<u8>, value: U16Fixed16) {
    out.extend_from_slice(&value.0.to_be_bytes());
}

/// Appends an `XYZNumber` (three `s15Fixed16`).
pub(crate) fn push_xyz_number(out: &mut Vec<u8>, value: XyzNumber) {
    push_s15fixed16(out, value.x);
    push_s15fixed16(out, value.y);
    push_s15fixed16(out, value.z);
}

/// Appends a 32-byte NUL-padded 7-bit-ASCII name field (the colorant/named-colour name encoding,
/// §10.5/§10.17), validating instead of truncating.
///
/// Rejects non-ASCII text, interior NULs (the decoder stops at the first NUL, so they cannot
/// round-trip), and names longer than the field. A name of exactly 32 bytes fills the field with no
/// terminator — non-conformant, but what the lenient decoder accepts, so such profiles round-trip.
pub(crate) fn push_ascii_32(out: &mut Vec<u8>, text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    if !text.is_ascii() || bytes.contains(&0) {
        return Err(IccError::Malformed(
            "icc: name field must be NUL-free ASCII",
        ));
    }
    if bytes.len() > 32 {
        return Err(IccError::Malformed("icc: name exceeds its 32-byte field"));
    }
    let mut field = [0u8; 32];
    field[..bytes.len()].copy_from_slice(bytes);
    out.extend_from_slice(&field);
    Ok(())
}

/// Appends a `dateTimeNumber` (six big-endian `u16`).
pub(crate) fn push_date_time(out: &mut Vec<u8>, value: DateTime) {
    for field in [
        value.year,
        value.month,
        value.day,
        value.hours,
        value.minutes,
        value.seconds,
    ] {
        out.extend_from_slice(&field.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_big_endian_scalars_in_order() {
        let buf = [
            0x12, // u8
            0x34, 0x56, // u16
            0x78, 0x9a, 0xbc, 0xde, // u32
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u64
        ];
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.u8().unwrap(), 0x12);
        assert_eq!(r.u16().unwrap(), 0x3456);
        assert_eq!(r.u32().unwrap(), 0x789a_bcde);
        assert_eq!(r.u64().unwrap(), u64::MAX);
    }

    #[test]
    fn reads_composite_icc_types() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"acsp"); // signature
        buf.extend_from_slice(&0xFFFF_0000_u32.to_be_bytes()); // s15fixed16 == -1.0
        for v in [1u16, 2, 3, 4, 5, 6] {
            buf.extend_from_slice(&v.to_be_bytes()); // date-time
        }
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.signature().unwrap(), Signature(*b"acsp"));
        assert_eq!(r.s15fixed16().unwrap().to_f64(), -1.0);
        assert_eq!(
            r.date_time().unwrap(),
            DateTime {
                year: 1,
                month: 2,
                day: 3,
                hours: 4,
                minutes: 5,
                seconds: 6,
            }
        );
    }

    #[test]
    fn bytes_and_skip_advance_the_cursor() {
        let buf = [10u8, 20, 30, 40, 50];
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.bytes(2).unwrap(), &[10, 20]);
        r.skip(1).unwrap();
        assert_eq!(r.bytes(2).unwrap(), &[40, 50]);
    }

    #[test]
    fn reads_past_end_error() {
        let buf = [0u8; 3];
        let mut r = ByteReader::new(&buf);
        assert!(r.u32().is_err());
        // A partial read does not advance the cursor past the failure point.
        assert_eq!(r.u16().unwrap(), 0);
    }

    #[test]
    fn xyz_number_decodes_three_components() {
        let mut buf = Vec::new();
        for raw in [0x0000_F6D6_i32, 0x0001_0000, 0x0000_D32D] {
            buf.extend_from_slice(&raw.to_be_bytes());
        }
        let xyz = ByteReader::new(&buf).xyz_number().unwrap();
        assert_eq!(xyz.x, S15Fixed16(0x0000_F6D6));
        assert_eq!(xyz.y, S15Fixed16(0x0001_0000));
        assert_eq!(xyz.z, S15Fixed16(0x0000_D32D));
    }

    #[test]
    fn at_offset_positions_and_bounds_checks() {
        let buf = [0u8, 0, 0xAB, 0xCD];
        assert_eq!(ByteReader::at(&buf, 2).unwrap().u16().unwrap(), 0xABCD);
        assert!(ByteReader::at(&buf, 5).is_err()); // offset past end
        assert!(ByteReader::at(&buf, 4).unwrap().u8().is_err()); // at end, nothing to read
    }
}
