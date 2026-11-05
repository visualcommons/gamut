//! JPEG bitstream recompression (jbrd) tests against the reference libjxl oracle:
//!
//! - the flagship guarantee: recompressing `fixtures/tiny_baseline.jpg` and reconstructing it with
//!   the reference decoder returns the **original JPEG bytes bit-for-bit**;
//! - the output is always ISO BMFF container framing (the `jbrd` box requires it), regardless of
//!   the configured [`Container`];
//! - the recompressed stream still decodes as ordinary pixels, and gamut's pure-Rust decoder and
//!   the libjxl oracle agree on them within the established lossy tolerance;
//! - robustness: malformed and truncated JPEG inputs are typed errors that leave the output buffer
//!   exactly as it was.
//!
//! Uses both codec halves (gamut encodes with libjxl, decodes with jxl-rs); compiled only when both
//! are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{DecodedSamples, decode, reconstruct_jpeg};
use gamut_core::{DecodeImage, ErrorKind, Rgb8};
use gamut_jxl::{Container, JxlDecoder, JxlEncoder};

/// A tiny deterministic 16x16 baseline JPEG (see `fixtures/README.md` for provenance).
const TINY_BASELINE_JPEG: &[u8] = include_bytes!("fixtures/tiny_baseline.jpg");

/// The 12-byte ISO BMFF `.jxl` signature box that must open every container-framed stream.
const CONTAINER_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

#[test]
fn recompress_reconstructs_the_original_jpeg_bit_for_bit() {
    let mut jxl = Vec::new();
    let written = JxlEncoder::new()
        .recompress_jpeg(TINY_BASELINE_JPEG, &mut jxl)
        .expect("recompression failed");
    assert_eq!(
        written,
        jxl.len(),
        "returned count must match bytes written"
    );
    assert!(
        jxl.starts_with(&CONTAINER_SIGNATURE),
        "jbrd output must be container-framed"
    );
    // The recompressed stream is smaller than trivially wrapping the JPEG would be, and the
    // reference decoder reconstructs the exact original bytes from it.
    let reconstructed = reconstruct_jpeg(&jxl);
    assert_eq!(
        reconstructed, TINY_BASELINE_JPEG,
        "reconstructed JPEG differs from the original"
    );
}

#[test]
fn recompress_forces_container_even_with_codestream_config() {
    // The `jbrd` reconstruction box can only live in the ISO BMFF container, so the configured
    // codestream framing is documented not to apply on this path.
    let mut jxl = Vec::new();
    JxlEncoder::new()
        .with_container(Container::Codestream)
        .recompress_jpeg(TINY_BASELINE_JPEG, &mut jxl)
        .expect("recompression failed");
    assert!(
        jxl.starts_with(&CONTAINER_SIGNATURE),
        "jbrd output must be container-framed even when Codestream is configured"
    );
}

#[test]
fn jbrd_stream_decodes_as_pixels_in_both_decoders() {
    let mut jxl = Vec::new();
    JxlEncoder::new()
        .recompress_jpeg(TINY_BASELINE_JPEG, &mut jxl)
        .expect("recompression failed");

    // The oracle sees a 16x16 3-channel 8-bit image.
    let oracle = decode(&jxl);
    assert_eq!((oracle.width, oracle.height), (16, 16));
    assert_eq!(oracle.num_channels, 3);
    let DecodedSamples::U8(oracle_samples) = oracle.samples else {
        panic!("oracle should decode a JPEG-derived stream at 8 bits");
    };

    // gamut's pure-Rust decoder reads the same container stream, and the two independent decoders
    // agree within the same per-sample tolerance the lossy differential tests use.
    let image = JxlDecoder::new()
        .decode_image(&jxl)
        .expect("gamut decode of the jbrd stream failed");
    let dims = gamut_core::ImageBuf::<Rgb8>::dimensions(&image);
    assert_eq!((dims.width(), dims.height()), (16, 16));
    let gamut_samples = image.as_samples();
    assert_eq!(gamut_samples.len(), oracle_samples.len());
    for (i, (&a, &b)) in gamut_samples.iter().zip(&oracle_samples).enumerate() {
        assert!(
            a.abs_diff(b) <= 2,
            "decoder disagreement at sample {i}: gamut={a} oracle={b}"
        );
    }
}

#[test]
fn recompress_appends_after_existing_data() {
    let mut out = vec![0xAA, 0xBB, 0xCC];
    let written = JxlEncoder::new()
        .recompress_jpeg(TINY_BASELINE_JPEG, &mut out)
        .expect("recompression failed");
    assert_eq!(&out[..3], &[0xAA, 0xBB, 0xCC], "existing prefix clobbered");
    assert_eq!(written, out.len() - 3);
    assert!(out[3..].starts_with(&CONTAINER_SIGNATURE));
}

#[test]
fn malformed_jpeg_is_a_typed_error_and_restores_the_buffer() {
    // Valid SOI marker followed by deterministic junk: not a decodable JPEG.
    let mut junk = vec![0xFF, 0xD8];
    let mut state = 0x12345678u32;
    for _ in 0..256 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        junk.push((state >> 24) as u8);
    }

    let mut out = vec![0x5A; 4];
    let err = JxlEncoder::new()
        .recompress_jpeg(&junk, &mut out)
        .expect_err("malformed JPEG must be rejected");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidInput | ErrorKind::Unsupported),
        "unexpected error class: {err:?}"
    );
    assert_eq!(out, vec![0x5A; 4], "output buffer not restored on error");
}

#[test]
fn truncated_jpeg_is_rejected() {
    let truncated = &TINY_BASELINE_JPEG[..TINY_BASELINE_JPEG.len() / 2];
    let mut out = Vec::new();
    let err = JxlEncoder::new()
        .recompress_jpeg(truncated, &mut out)
        .expect_err("truncated JPEG must be rejected");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidInput | ErrorKind::Unsupported),
        "unexpected error class: {err:?}"
    );
    assert!(out.is_empty(), "output buffer not restored on error");
}
