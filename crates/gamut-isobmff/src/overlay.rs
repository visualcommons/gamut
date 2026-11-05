//! The `iovl` derived-image payload (ISO/IEC 23008-12 §6.6.2.4.2 `ImageOverlay`).
//!
//! An `iovl` item composes several coded images onto a single canvas: its
//! [`references`](crate::Item::references) carry a `dimg` entry listing the composed items in
//! placement order, and its [`payload`](crate::Item::payload) is an `ImageOverlay` struct giving
//! the canvas size, its background fill colour, and each input's top-left offset on the canvas. The
//! container models the reference and the payload bytes structurally; this helper is the opt-in
//! parser/serialiser for the payload, so a consumer that needs the overlay geometry does not have
//! to hand-decode the bytes. The composed image payloads themselves stay opaque to this crate.
//!
//! The wire layout (§6.6.2.4.2) is:
//!
//! ```text
//! version           u8    (= 0)
//! flags             u8    (bit 0 selects the field length: 0 → 16-bit, 1 → 32-bit)
//! canvas_fill_value u16 × 4   (R, G, B, A — the background the canvas is cleared to)
//! FieldLength = ((flags & 1) + 1) * 16   → 16- or 32-bit
//! output_width      u(FieldLength)          (canvas width,  unsigned)
//! output_height     u(FieldLength)          (canvas height, unsigned)
//! for i in 0..reference_count {             (one per `dimg` reference, in order)
//!     horizontal_offset s(FieldLength)      (top-left x on the canvas, SIGNED)
//!     vertical_offset   s(FieldLength)      (top-left y on the canvas, SIGNED)
//! }
//! ```
//!
//! `reference_count` is *not* stored in the payload — it is implied by the number of `dimg`
//! references — so [`ImageOverlay::parse`] takes it as a parameter and rejects a payload that does
//! not contain exactly that many offset pairs (truncated or with trailing bytes).

use gamut_core::{Error, Result};

use crate::boxes::{BoxBuilder, BoxReader};

/// The parsed `ImageOverlay` payload of an `iovl` item (ISO/IEC 23008-12 §6.6.2.4.2): the canvas
/// size, its background fill colour, and each composed input's top-left offset on the canvas.
///
/// The `dimg` item references list the composed inputs themselves (in the same order as
/// [`offsets`](Self::offsets)); this struct only carries the geometry. Round-trips through
/// [`ImageOverlay::to_bytes`] and [`ImageOverlay::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOverlay {
    /// The background colour the canvas is cleared to before compositing, as `[R, G, B, A]` 16-bit
    /// samples (`canvas_fill_value`).
    pub canvas_fill_value: [u16; 4],
    /// Width of the composited canvas in pixels (`output_width`).
    pub output_width: u32,
    /// Height of the composited canvas in pixels (`output_height`).
    pub output_height: u32,
    /// Each composed input's top-left position on the canvas, in `dimg` reference order.
    pub offsets: Vec<OverlayOffset>,
}

/// One composed input's top-left position on an [`ImageOverlay`] canvas (the
/// `horizontal_offset`/`vertical_offset` pair of ISO/IEC 23008-12 §6.6.2.4.2).
///
/// Offsets are signed: a negative value places the input off the left or top edge (it is clipped
/// to the canvas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OverlayOffset {
    /// The input's left edge on the canvas (`horizontal_offset`).
    pub horizontal: i32,
    /// The input's top edge on the canvas (`vertical_offset`).
    pub vertical: i32,
}

impl ImageOverlay {
    /// Parses an `iovl` item payload. `reference_count` is the number of `dimg` references the item
    /// carries — the payload stores exactly that many offset pairs and nothing else.
    ///
    /// # Errors
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a
    /// non-zero `version` (only version 0 is defined), and an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the payload is
    /// truncated (fewer than `reference_count` offset pairs) or carries trailing bytes (more than `reference_count`).
    pub fn parse(data: &[u8], reference_count: usize) -> Result<Self> {
        let mut r = BoxReader::new(data);
        let version = r.u8()?;
        if version != 0 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iovl version (only v0)",
            ));
        }
        let flags = r.u8()?;
        // FieldLength = ((flags & 1) + 1) * 16 bits — 16-bit fields unless flag bit 0 selects 32.
        let wide = flags & 1 == 1;
        let mut canvas_fill_value = [0u16; 4];
        for sample in &mut canvas_fill_value {
            *sample = r.u16()?;
        }
        let (output_width, output_height) = if wide {
            (r.u32()?, r.u32()?)
        } else {
            (u32::from(r.u16()?), u32::from(r.u16()?))
        };
        // `reference_count` comes from the already-parsed `dimg` list, not the untrusted stream, but
        // the bounded reads below still fail cleanly on a payload too short for it.
        let mut offsets = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            let horizontal = read_signed(&mut r, wide)?;
            let vertical = read_signed(&mut r, wide)?;
            offsets.push(OverlayOffset {
                horizontal,
                vertical,
            });
        }
        if r.remaining() != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iovl has trailing bytes",
            ));
        }
        Ok(Self {
            canvas_fill_value,
            output_width,
            output_height,
            offsets,
        })
    }

    /// Serialises this overlay to an `iovl` item payload, choosing the compact 16-bit field form
    /// when every value fits and the 32-bit form otherwise.
    ///
    /// Because the offsets are *signed*, the 16-bit form is used iff both canvas dimensions fit in
    /// [`u16`] **and** every offset component fits in [`i16`]; if any dimension exceeds `u16::MAX`
    /// or any offset falls outside `i16::MIN..=i16::MAX`, the 32-bit form is used instead. The
    /// composed-input count is implied by [`offsets`](Self::offsets) and never written.
    ///
    /// # Errors
    /// Currently infallible — every field is representable in one of the two wire forms — but
    /// returns [`Result`] to mirror [`ImageGrid::to_bytes`](crate::ImageGrid::to_bytes) and remain
    /// forward-compatible if a future MIAF constraint adds a validation step.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let dims_fit =
            self.output_width <= u32::from(u16::MAX) && self.output_height <= u32::from(u16::MAX);
        let offsets_fit = self
            .offsets
            .iter()
            .all(|o| i16::try_from(o.horizontal).is_ok() && i16::try_from(o.vertical).is_ok());
        let wide = !(dims_fit && offsets_fit);

        let mut b = BoxBuilder::new();
        b.u8(0); // version
        b.u8(u8::from(wide)); // flags: bit 0 selects the 32-bit field form
        for &sample in &self.canvas_fill_value {
            b.u16(sample);
        }
        if wide {
            b.u32(self.output_width);
            b.u32(self.output_height);
        } else {
            b.u16(self.output_width as u16);
            b.u16(self.output_height as u16);
        }
        for &OverlayOffset {
            horizontal,
            vertical,
        } in &self.offsets
        {
            if wide {
                b.u32(horizontal as u32);
                b.u32(vertical as u32);
            } else {
                b.u16(horizontal as i16 as u16);
                b.u16(vertical as i16 as u16);
            }
        }
        Ok(b.into_vec())
    }
}

/// Reads one signed `FieldLength` offset: a 16-bit value sign-extended to [`i32`] in the compact
/// form, or a full 32-bit value in the wide form.
fn read_signed(r: &mut BoxReader, wide: bool) -> Result<i32> {
    if wide {
        Ok(r.u32()? as i32)
    } else {
        Ok(i32::from(r.u16()? as i16))
    }
}
