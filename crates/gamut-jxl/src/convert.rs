//! Reassembly of jxl-rs's raw output bytes into typed samples, and the layout tag that describes
//! them.
//!
//! jxl-rs writes the decoded frame as a flat **byte** buffer of interleaved samples in the stream's
//! *natural* colour layout (grayscale or RGB, optionally followed by one interleaved alpha
//! channel), in native byte order at the bit width the [`crate::decoder`] requested (8- or 16-bit).
//!
//! Turning those bytes into a caller's requested [`gamut_core::Pixel`] layout is two steps, and only
//! the first is jxl's business:
//!
//! 1. **Byte → sample reassembly** (here). Native-endian, and specific to jxl-rs's byte-oriented
//!    output — no other gamut decoder hands back untyped bytes.
//! 2. **Layout conversion** — grayscale → RGB expansion, alpha padding or dropping, depth changes.
//!    That is not jxl-specific at all, so it is [`gamut_core::convert`]'s job, and this crate does
//!    not restate its rules.
//!
//! Step 1 costs one pass and one intermediate allocation the previous fused implementation avoided.
//! That is the deliberate price of having a single conversion implementation in the workspace: the
//! entropy decode dominates, and the alternative is reinterpreting `&[u8]` as `&[u16]`, which needs
//! `unsafe`.

use gamut_core::{PixelFormat, Sample};

/// A decoded-sample primitive that can be rebuilt from jxl-rs's native-endian output bytes.
///
/// Sealed in practice to `u8` and `u16` by its [`Sample`] supertrait — the only sample widths gamut
/// and jxl-rs exchange.
pub(crate) trait ConvSample: Sample {
    /// Bytes per sample in the raw jxl-rs output buffer.
    const BYTES: usize;

    /// Reassembles one sample from the first [`ConvSample::BYTES`] bytes of `bytes`, interpreting a
    /// multi-byte sample in native byte order (jxl-rs is configured to emit native endianness).
    fn from_ne_bytes(bytes: &[u8]) -> Self;
}

impl ConvSample for u8 {
    const BYTES: usize = 1;

    fn from_ne_bytes(bytes: &[u8]) -> Self {
        bytes[0]
    }
}

impl ConvSample for u16 {
    const BYTES: usize = 2;

    fn from_ne_bytes(bytes: &[u8]) -> Self {
        u16::from_ne_bytes([bytes[0], bytes[1]])
    }
}

/// Reassembles every sample in `src` at `S`'s width, preserving the interleaving untouched.
///
/// Trailing bytes that cannot form a whole sample are ignored; the [`crate::decoder`] sizes the
/// buffer from the frame geometry, so there are none in practice.
pub(crate) fn reassemble<S: ConvSample>(src: &[u8]) -> Vec<S> {
    src.chunks_exact(S::BYTES).map(S::from_ne_bytes).collect()
}

/// The [`PixelFormat`] describing jxl-rs's natural output layout for a frame.
///
/// `color` is 1 (grayscale) or 3 (RGB) as the stream carries it, `alpha` whether an interleaved
/// alpha sample follows, and `bytes_per_sample` the width the decoder asked jxl-rs for. Returns
/// `None` for a combination outside gamut's pixel matrix, which the caller treats as unsupported.
pub(crate) fn native_format(
    color: usize,
    alpha: bool,
    bytes_per_sample: usize,
) -> Option<PixelFormat> {
    Some(match (color, alpha, bytes_per_sample) {
        (1, false, 1) => PixelFormat::Gray8,
        (1, true, 1) => PixelFormat::GrayAlpha8,
        (3, false, 1) => PixelFormat::Rgb8,
        (3, true, 1) => PixelFormat::Rgba8,
        (1, false, 2) => PixelFormat::Gray16,
        (1, true, 2) => PixelFormat::GrayAlpha16,
        (3, false, 2) => PixelFormat::Rgb16,
        (3, true, 2) => PixelFormat::Rgba16,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_from_ne_bytes_reads_first_byte() {
        assert_eq!(<u8 as ConvSample>::from_ne_bytes(&[0xAB, 0xCD]), 0xAB);
        assert_eq!(<u8 as ConvSample>::BYTES, 1);
    }

    #[test]
    fn u16_from_ne_bytes_reads_a_native_endian_pair() {
        let value = 0xABCDu16;
        let bytes = value.to_ne_bytes();
        assert_eq!(<u16 as ConvSample>::from_ne_bytes(&bytes), value);
        assert_eq!(<u16 as ConvSample>::BYTES, 2);
        // Only the first two bytes participate, whatever follows them.
        let padded = [bytes[0], bytes[1], 0xFF, 0xFF];
        assert_eq!(<u16 as ConvSample>::from_ne_bytes(&padded), value);
    }

    #[test]
    fn reassemble_preserves_order_and_width() {
        assert_eq!(reassemble::<u8>(&[1, 2, 3]), vec![1u8, 2, 3]);

        // Two distinct 16-bit samples, so a swapped pair or a stride error is visible.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x1234u16.to_ne_bytes());
        bytes.extend_from_slice(&0x5678u16.to_ne_bytes());
        assert_eq!(reassemble::<u16>(&bytes), vec![0x1234u16, 0x5678]);
    }

    #[test]
    fn reassemble_ignores_an_incomplete_trailing_sample() {
        let bytes = [0x11, 0x22, 0x33];
        assert_eq!(reassemble::<u16>(&bytes).len(), 1);
    }

    #[test]
    fn native_format_covers_every_layout_jxl_can_emit() {
        // The eight combinations the decoder can request, each a distinct gamut layout.
        let expected = [
            ((1, false, 1), PixelFormat::Gray8),
            ((1, true, 1), PixelFormat::GrayAlpha8),
            ((3, false, 1), PixelFormat::Rgb8),
            ((3, true, 1), PixelFormat::Rgba8),
            ((1, false, 2), PixelFormat::Gray16),
            ((1, true, 2), PixelFormat::GrayAlpha16),
            ((3, false, 2), PixelFormat::Rgb16),
            ((3, true, 2), PixelFormat::Rgba16),
        ];
        for ((color, alpha, width), format) in expected {
            assert_eq!(native_format(color, alpha, width), Some(format));
            // The tag must agree with the layout it claims to describe.
            assert_eq!(format.channels(), color + usize::from(alpha));
            assert_eq!(format.bytes_per_sample(), width);
        }
        // Anything outside gamut's matrix is reported rather than guessed at.
        assert_eq!(native_format(4, false, 1), None);
        assert_eq!(native_format(3, false, 4), None);
        assert_eq!(native_format(0, false, 1), None);
    }
}
