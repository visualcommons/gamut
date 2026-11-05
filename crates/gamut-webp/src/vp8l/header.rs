//! VP8L bitstream header: signature, dimensions, and feature hints (RFC 9649 §3.4).

use gamut_core::{Dimensions, Error, Result};

use crate::vp8l::bit_io::{BitReader, BitWriter};

/// The byte that begins every VP8L bitstream (RFC 9649 §3.4).
pub const VP8L_SIGNATURE: u8 = 0x2f;

/// Maximum VP8L image dimension (the 14-bit width/height fields encode `1..=16384`).
pub const VP8L_MAX_DIMENSION: u16 = 1 << 14;

/// A decoded VP8L bitstream header (RFC 9649 §3.4).
///
/// On the wire the [`VP8L_SIGNATURE`] byte is followed by 14-bit `width - 1` and `height - 1`
/// fields, a 1-bit alpha hint, and a 3-bit version (the only defined value is 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp8lHeader {
    /// Image width in pixels (`1..=16384`; stored on the wire as `width - 1`).
    pub width: u16,
    /// Image height in pixels (`1..=16384`; stored on the wire as `height - 1`).
    pub height: u16,
    /// The `alpha_is_used` hint: whether any pixel has a non-opaque alpha value.
    pub alpha_is_used: bool,
    /// Bitstream version number; the only defined value is 0.
    pub version: u8,
}

impl Vp8lHeader {
    /// Builds a header for `dims` (version 0), validating the dimensions against the 14-bit field
    /// range.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if either
    /// dimension exceeds [`VP8L_MAX_DIMENSION`] (16384). A zero side cannot reach here:
    /// [`Dimensions::new`] rejects it.
    pub fn from_dimensions(dims: Dimensions, alpha_is_used: bool) -> Result<Self> {
        let max = u32::from(VP8L_MAX_DIMENSION);
        if dims.width() > max || dims.height() > max {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: dimensions out of range (1..=16384)",
            ));
        }
        Ok(Self {
            width: dims.width() as u16,
            height: dims.height() as u16,
            alpha_is_used,
            version: 0,
        })
    }

    /// Returns the header's dimensions.
    #[cfg(test)]
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        Dimensions::new(u32::from(self.width), u32::from(self.height)).unwrap()
    }

    /// Reads a VP8L header: the [`VP8L_SIGNATURE`] byte, 14-bit `width - 1` / `height - 1`, the
    /// 1-bit alpha hint, and the 3-bit version (RFC 9649 §3.4).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// signature byte is wrong, the version is nonzero, or the stream is truncated.
    pub fn read(r: &mut BitReader<'_>) -> Result<Self> {
        if r.read_bits(8)? != u32::from(VP8L_SIGNATURE) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: bad signature byte",
            ));
        }
        // 14-bit fields store width-1/height-1, so the decoded values are always 1..=16384.
        let width = (r.read_bits(14)? + 1) as u16;
        let height = (r.read_bits(14)? + 1) as u16;
        let alpha_is_used = r.read_bits(1)? != 0;
        let version = r.read_bits(3)? as u8;
        if version != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: unsupported version (must be 0)",
            ));
        }
        Ok(Self {
            width,
            height,
            alpha_is_used,
            version,
        })
    }

    /// Writes this header to `w` (the inverse of [`read`](Self::read)).
    pub fn write(&self, w: &mut BitWriter) {
        w.write_bits(u32::from(VP8L_SIGNATURE), 8);
        w.write_bits(u32::from(self.width.saturating_sub(1)), 14);
        w.write_bits(u32::from(self.height.saturating_sub(1)), 14);
        w.write_bits(u32::from(self.alpha_is_used), 1);
        w.write_bits(u32::from(self.version), 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(header: Vp8lHeader) -> Vp8lHeader {
        let mut w = BitWriter::new();
        header.write(&mut w);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        Vp8lHeader::read(&mut r).expect("valid header")
    }

    #[test]
    fn round_trips_min_and_max_dimensions() {
        for (w, h, alpha) in [(1u16, 1u16, false), (16384, 16384, true), (17, 9, false)] {
            let header = Vp8lHeader {
                width: w,
                height: h,
                alpha_is_used: alpha,
                version: 0,
            };
            assert_eq!(round_trip(header), header);
        }
    }

    #[test]
    fn from_dimensions_validates_range() {
        assert!(Vp8lHeader::from_dimensions(Dimensions::new(1, 1).unwrap(), false).is_ok());
        let max = Dimensions::new(16384, 16384).unwrap();
        assert_eq!(
            Vp8lHeader::from_dimensions(max, true).unwrap().dimensions(),
            max
        );
        // A zero side can no longer be expressed as a `Dimensions` at all: `Dimensions::new`
        // rejects it before `from_dimensions` is reached.
        for (width, height) in [(0, 5), (5, 0), (16385, 1), (1, 16385)] {
            assert!(matches!(
                Dimensions::new(width, height).and_then(|bad| Vp8lHeader::from_dimensions(bad, false)),
                Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
            ));
        }
    }

    #[test]
    fn rejects_bad_signature() {
        let bytes = [0x2e, 0, 0, 0, 0];
        let mut r = BitReader::new(&bytes);
        assert!(matches!(
            Vp8lHeader::read(&mut r),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn rejects_nonzero_version() {
        // Build a header with version 1 by hand and confirm read rejects it.
        let mut w = BitWriter::new();
        w.write_bits(u32::from(VP8L_SIGNATURE), 8);
        w.write_bits(0, 14);
        w.write_bits(0, 14);
        w.write_bits(0, 1);
        w.write_bits(1, 3); // version = 1
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert!(matches!(
            Vp8lHeader::read(&mut r),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn rejects_truncated_header() {
        let mut r = BitReader::new(&[0x2f, 0x00]); // signature + only 8 of 28 dimension bits
        assert!(matches!(
            Vp8lHeader::read(&mut r),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }
}
