//! Orientation-signalling differential tests against the reference libjxl oracle:
//!
//! - for **all eight** EXIF orientations, gamut's pure-Rust decoder and the libjxl oracle produce
//!   bit-identical display-oriented output from the same stream (both apply the transform);
//! - the four transposing orientations swap the displayed dimensions, the other four keep them;
//! - the transform direction is pinned by hand for [`Orientation::Rotate180`] (decoders must
//!   *undo* the signalled orientation, not apply it forwards — for 180° both are identical, but
//!   the sample-order reversal proves the pixels actually moved);
//! - signalling [`Orientation::Identity`] is byte-identical to not signalling anything.
//!
//! Uses both codec halves; compiled only when both are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{DecodedSamples, decode, gen_u8};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_jxl::{JxlDecoder, JxlEncoder, Orientation};

/// Coded dimensions: deliberately non-square so a missed transpose is loud.
const W: u32 = 5;
const H: u32 = 3;

const ALL: [Orientation; 8] = [
    Orientation::Identity,
    Orientation::FlipHorizontal,
    Orientation::Rotate180,
    Orientation::FlipVertical,
    Orientation::Transpose,
    Orientation::Rotate90Cw,
    Orientation::AntiTranspose,
    Orientation::Rotate90Ccw,
];

/// Encodes the deterministic W x H RGB8 pattern losslessly with the given orientation.
fn encode_oriented(orientation: Orientation) -> Vec<u8> {
    let dims = Dimensions::new(W, H).unwrap();
    let samples = gen_u8(W, H, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    JxlEncoder::lossless()
        .with_orientation(orientation)
        .encode_image(image, &mut out)
        .expect("encode failed");
    out
}

#[test]
fn all_orientations_decode_identically_in_gamut_and_oracle() {
    for orientation in ALL {
        let jxl = encode_oriented(orientation);

        let oracle = decode(&jxl);
        let DecodedSamples::U8(oracle_samples) = &oracle.samples else {
            panic!("oracle should decode 8-bit for {orientation:?}");
        };

        let image: ImageBuf<Rgb8> = JxlDecoder::new()
            .decode_image(&jxl)
            .unwrap_or_else(|e| panic!("gamut decode failed for {orientation:?}: {e:?}"));
        let dims = image.dimensions();

        // Both decoders present display orientation: identical dims and identical samples.
        assert_eq!(
            (dims.width(), dims.height()),
            (oracle.width, oracle.height),
            "dims disagree for {orientation:?}"
        );
        assert_eq!(
            image.as_samples(),
            oracle_samples.as_slice(),
            "pixels disagree for {orientation:?}"
        );
    }
}

#[test]
fn transposing_orientations_swap_displayed_dimensions() {
    for orientation in ALL {
        let jxl = encode_oriented(orientation);
        let image: ImageBuf<Rgb8> = JxlDecoder::new()
            .decode_image(&jxl)
            .unwrap_or_else(|e| panic!("gamut decode failed for {orientation:?}: {e:?}"));
        let dims = image.dimensions();
        let expected = if orientation.transposes() {
            (H, W)
        } else {
            (W, H)
        };
        assert_eq!(
            (dims.width(), dims.height()),
            expected,
            "displayed dims wrong for {orientation:?}"
        );
    }
}

#[test]
fn rotate_180_reverses_the_pixel_order() {
    // For a 180-degree rotation the display image is the coded image with its pixels in exactly
    // reverse order (per-pixel RGB triplets kept intact). Hand-computing this pins the transform
    // direction independent of the oracle.
    let source = gen_u8(W, H, 3);
    let jxl = encode_oriented(Orientation::Rotate180);
    let image: ImageBuf<Rgb8> = JxlDecoder::new()
        .decode_image(&jxl)
        .expect("gamut decode failed");

    let mut expected = Vec::with_capacity(source.len());
    for pixel in source.as_chunks::<3>().0.iter().rev() {
        expected.extend_from_slice(pixel);
    }
    assert_eq!(image.as_samples(), expected.as_slice());
}

#[test]
fn identity_orientation_is_byte_identical_to_default() {
    let dims = Dimensions::new(W, H).unwrap();
    let samples = gen_u8(W, H, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut plain = Vec::new();
    JxlEncoder::lossless()
        .encode_image(image, &mut plain)
        .unwrap();
    assert_eq!(
        encode_oriented(Orientation::Identity),
        plain,
        "explicit Identity must not change the stream"
    );
}
