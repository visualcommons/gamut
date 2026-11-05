//! The `grid` derived-image payload (ISO/IEC 23008-12 §6.6.2.3.2 `ImageGrid`).
//!
//! A `grid` item assembles a single large image from a matrix of coded tiles: its
//! [`references`](crate::Item::references) carry a `dimg` entry listing the tile items in
//! row-major order, and its [`payload`](crate::Item::payload) is an `ImageGrid` struct giving the
//! tile grid shape and the assembled output dimensions. The container models the reference and the
//! payload bytes structurally; this helper is the opt-in parser/serialiser for the payload, so a
//! consumer that needs the grid geometry does not have to hand-decode the bytes. The tile payloads
//! themselves stay opaque to this crate.

use gamut_core::{Error, Result};

use crate::boxes::{BoxBuilder, BoxReader};

/// The parsed `ImageGrid` payload of a `grid` item (ISO/IEC 23008-12 §6.6.2.3.2): the tile matrix
/// dimensions and the assembled output size.
///
/// The `dimg` item references list the tile items themselves (row-major); this struct only carries
/// the geometry. Round-trips through [`ImageGrid::to_bytes`] and [`ImageGrid::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageGrid {
    /// Number of tile rows, `1..=256` (stored on the wire as `rows_minus_one`).
    pub rows: u16,
    /// Number of tile columns, `1..=256` (stored on the wire as `columns_minus_one`).
    pub columns: u16,
    /// Width of the assembled output image in pixels (may exceed the tiled area, which is cropped).
    pub output_width: u32,
    /// Height of the assembled output image in pixels (may exceed the tiled area, which is cropped).
    pub output_height: u32,
}

impl ImageGrid {
    /// Parses a `grid` item payload.
    ///
    /// # Errors
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a
    /// non-zero `version` (only version 0 is defined), and an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the payload is
    /// truncated or carries trailing bytes.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = BoxReader::new(data);
        let version = r.u8()?;
        if version != 0 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: grid version (only v0)",
            ));
        }
        let flags = r.u8()?;
        let rows = u16::from(r.u8()?) + 1;
        let columns = u16::from(r.u8()?) + 1;
        // FieldLength = ((flags & 1) + 1) * 16 bits — 16-bit dims unless flag bit 0 selects 32-bit.
        let (output_width, output_height) = if flags & 1 == 0 {
            (u32::from(r.u16()?), u32::from(r.u16()?))
        } else {
            (r.u32()?, r.u32()?)
        };
        if r.remaining() != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: grid has trailing bytes",
            ));
        }
        Ok(Self {
            rows,
            columns,
            output_width,
            output_height,
        })
    }

    /// Serialises this grid to a `grid` item payload, choosing the 16-bit output-dimension form
    /// when both dimensions fit and the 32-bit form otherwise.
    ///
    /// # Errors
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// [`rows`](Self::rows) or [`columns`](Self::columns) is outside `1..=256` — the wire format stores them as an 8-bit `minus_one`, so a larger count
    /// cannot be represented (rather than silently truncating it).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if !(1..=256).contains(&self.rows) || !(1..=256).contains(&self.columns) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: grid rows/columns out of range (1..=256)",
            ));
        }
        let wide =
            self.output_width > u32::from(u16::MAX) || self.output_height > u32::from(u16::MAX);
        let mut b = BoxBuilder::new();
        b.u8(0); // version
        b.u8(u8::from(wide)); // flags: bit 0 selects the 32-bit output-dimension form
        b.u8((self.rows - 1) as u8);
        b.u8((self.columns - 1) as u8);
        if wide {
            b.u32(self.output_width);
            b.u32(self.output_height);
        } else {
            b.u16(self.output_width as u16);
            b.u16(self.output_height as u16);
        }
        Ok(b.into_vec())
    }
}
