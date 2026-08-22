//! Decoder robustness corpus: malformed, truncated, and adversarial inputs must yield typed errors
//! (or a valid result), never a panic. gamut ships a `#![forbid(unsafe_code)]` decoder so decoding
//! untrusted input cannot be *memory*-unsafe; this corpus additionally pins that no input drives it
//! to *panic* (which would be a denial-of-service on a server decoding user uploads).

use gamut_core::{DecodeImage, ImageBuf, Rgb8};
use gamut_webp::WebpDecoder;
use gamut_webp::vp8::frame::{decode_frame, encode_frame};

/// A valid VP8 key-frame payload to mutate.
fn valid_payload() -> Vec<u8> {
    let yuv = gamut_color::Yuv420::new(
        48,
        32,
        (0..48 * 32).map(|i| (i % 251) as u8).collect(),
        vec![110u8; 24 * 16],
        vec![120u8; 24 * 16],
    )
    .unwrap();
    encode_frame(&yuv, 40)
        .expect("fixture fits the partition-size fields")
        .0
}

#[test]
fn vp8_decode_rejects_obviously_invalid_headers() {
    // Too short to hold the frame tag / uncompressed header, a zero header (bad start code), and an
    // all-ones header (the inverted key-frame bit marks it an unsupported interframe).
    for bad in [
        &[][..],
        &[0u8; 2][..],
        &[0u8; 9][..],
        &[0u8; 10][..],
        &[0xffu8; 10][..],
    ] {
        assert!(
            decode_frame(bad).is_err(),
            "{}-byte garbage must be rejected",
            bad.len()
        );
    }
}

#[test]
fn vp8_decode_survives_truncation() {
    let valid = valid_payload();
    assert!(decode_frame(&valid).is_ok(), "the baseline payload decodes");
    // Truncating the valid payload at every length must never panic (it may error or decode garbage).
    for len in 0..valid.len() {
        let _ = decode_frame(&valid[..len]);
    }
}

#[test]
fn vp8_decode_survives_bit_flips() {
    let valid = valid_payload();
    // Flip every bit of every byte except the 14-bit dimension fields (bytes 6..10, whose corruption
    // would only swap the valid frame for a differently-but-validly-sized one, ballooning the test's
    // memory). Corrupting the frame tag, start code, and the entire entropy-coded body must not panic.
    for i in (0..valid.len()).filter(|&i| !(6..10).contains(&i)) {
        for bit in 0..8u8 {
            let mut bad = valid.clone();
            bad[i] ^= 1 << bit;
            let _ = decode_frame(&bad);
        }
    }
}

#[test]
fn webp_decoder_rejects_non_webp_containers() {
    let dec = WebpDecoder::new();
    for bad in [
        &b""[..],
        &b"RIFF"[..],
        &b"RIFF\x04\x00\x00\x00WEBP"[..],
        &b"not a webp file"[..],
        &[0u8; 64][..],
        &[0xffu8; 64][..],
    ] {
        let r: gamut_core::Result<ImageBuf<Rgb8>> = dec.decode_image(bad);
        assert!(r.is_err(), "non-WebP input must be rejected");
    }
}
