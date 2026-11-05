//! [`DecodedFrame`]: the planar samples a codec backend returns and the planar surface delivers.

use gamut_color::ChromaSubsampling;
use gamut_core::{Dimensions, Error, Result};

/// The owned planar output of a codec backend: a YCbCr (or monochrome) frame at native chroma and
/// bit depth.
///
/// Samples are carried uniformly as `u16` whatever the `bit_depth` (an 8-bit frame simply uses
/// the low byte), so one plane layout serves 8..=16-bit content and maps cleanly onto a C buffer.
/// The planes are row-major: `y` is `width × height`; `cb`/`cr` are the chroma-plane dimensions
/// of the [`chroma`](Self::chroma) layout ([`ChromaSubsampling::chroma_dimensions`]) and are
/// **empty** for [`ChromaSubsampling::Cs400`].
///
/// Construct with [`DecodedFrame::new`], which validates every plane length against
/// `width`/`height`/`chroma` (chroma dimensions use ceiling division, so an odd luma dimension is
/// handled) and the `bit_depth` range — an internally inconsistent frame is unrepresentable, so
/// the pipeline downstream can trust the plane sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    dimensions: Dimensions,
    bit_depth: u8,
    chroma: ChromaSubsampling,
    y: Vec<u16>,
    cb: Vec<u16>,
    cr: Vec<u16>,
}

impl DecodedFrame {
    /// Builds a [`DecodedFrame`] from its planes, validating internal consistency.
    ///
    /// `width`/`height` are the luma dimensions the codec outputs (after its own cropping or
    /// super-resolution). The chroma planes `cb`/`cr` must match
    /// [`ChromaSubsampling::chroma_dimensions`] for `(width, height)` — **ceiling** division on
    /// the subsampled axes, so a 5×3 luma frame in 4:2:0 has 3×2 chroma planes — and must both be
    /// **empty** for [`ChromaSubsampling::Cs400`]. `bit_depth` must be in `8..=16` (the codecs
    /// code 8/10/12; the wider bound keeps the type usable for intermediate results).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// either dimension is zero, if `bit_depth` is outside `8..=16`, or if any plane length does
    /// not match the dimensions for `chroma`.
    pub fn new(
        dimensions: Dimensions,
        bit_depth: u8,
        chroma: ChromaSubsampling,
        y: Vec<u16>,
        cb: Vec<u16>,
        cr: Vec<u16>,
    ) -> Result<Self> {
        let luma = dimensions.num_pixels().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: frame dimensions overflow usize",
            )
        })?;
        if !(8..=16).contains(&bit_depth) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: frame bit depth out of range (8..=16)",
            ));
        }
        if y.len() != luma {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: luma plane length does not match dimensions",
            ));
        }
        let (cw, ch) = chroma.chroma_dimensions(dimensions.width(), dimensions.height());
        // Cannot overflow: each chroma dimension is at most its luma dimension, and `luma` fit
        // usize. Monochrome's `(0, 0)` makes this zero, so one comparison covers both shapes.
        let chroma_len = cw as usize * ch as usize;
        if cb.len() != chroma_len || cr.len() != chroma_len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: chroma plane length does not match dimensions",
            ));
        }
        Ok(Self {
            dimensions,
            bit_depth,
            chroma,
            y,
            cb,
            cr,
        })
    }

    /// The luma width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.dimensions.width()
    }

    /// The luma height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.dimensions.height()
    }

    /// The luma dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// The per-sample bit depth (`8..=16`). Samples are stored in `u16` regardless.
    #[must_use]
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// The chroma sampling layout.
    #[must_use]
    pub fn chroma(&self) -> ChromaSubsampling {
        self.chroma
    }

    /// The chroma-plane dimensions `(width, height)` — `(0, 0)` for
    /// [`ChromaSubsampling::Cs400`].
    #[must_use]
    pub fn chroma_dimensions(&self) -> (u32, u32) {
        self.chroma
            .chroma_dimensions(self.dimensions.width(), self.dimensions.height())
    }

    /// The row-major luma plane (`width × height` samples).
    #[must_use]
    pub fn y(&self) -> &[u16] {
        &self.y
    }

    /// The row-major Cb plane (empty for [`ChromaSubsampling::Cs400`]).
    #[must_use]
    pub fn cb(&self) -> &[u16] {
        &self.cb
    }

    /// The row-major Cr plane (empty for [`ChromaSubsampling::Cs400`]).
    #[must_use]
    pub fn cr(&self) -> &[u16] {
        &self.cr
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions::new(w, h).unwrap()
    }

    fn refusal(r: Result<DecodedFrame>) -> Option<&'static str> {
        let e = r.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidInput);
        e.static_message()
    }

    #[test]
    fn odd_luma_takes_ceiling_chroma_planes() {
        // 5×3 at 4:2:0 has 3×2 chroma planes.
        let frame = DecodedFrame::new(
            dims(5, 3),
            10,
            ChromaSubsampling::Cs420,
            vec![0; 15],
            vec![1; 6],
            vec![2; 6],
        )
        .unwrap();
        assert_eq!(frame.chroma_dimensions(), (3, 2));
        assert_eq!(
            (frame.width(), frame.height(), frame.bit_depth()),
            (5, 3, 10)
        );
        assert_eq!((frame.cb()[0], frame.cr()[0]), (1, 2));
    }

    #[test]
    fn the_bit_depth_must_lie_in_8_to_16() {
        let at = |depth| {
            DecodedFrame::new(
                dims(1, 1),
                depth,
                ChromaSubsampling::Cs400,
                vec![0],
                vec![],
                vec![],
            )
        };
        assert!(at(8).is_ok());
        assert!(at(16).is_ok());
        for depth in [7, 17] {
            assert_eq!(
                refusal(at(depth)),
                Some("HEIF: frame bit depth out of range (8..=16)")
            );
        }
    }

    #[test]
    fn a_plane_of_the_wrong_length_is_refused() {
        let frame = |y: usize, cb: usize, cr: usize, chroma| {
            DecodedFrame::new(dims(2, 2), 8, chroma, vec![0; y], vec![0; cb], vec![0; cr])
        };
        assert_eq!(
            refusal(frame(3, 1, 1, ChromaSubsampling::Cs420)),
            Some("HEIF: luma plane length does not match dimensions")
        );
        let chroma = Some("HEIF: chroma plane length does not match dimensions");
        assert_eq!(refusal(frame(4, 2, 1, ChromaSubsampling::Cs420)), chroma);
        assert_eq!(refusal(frame(4, 1, 2, ChromaSubsampling::Cs420)), chroma);
        // Monochrome carries no chroma at all.
        assert_eq!(refusal(frame(4, 1, 0, ChromaSubsampling::Cs400)), chroma);
        assert_eq!(refusal(frame(4, 0, 1, ChromaSubsampling::Cs400)), chroma);
    }
}
