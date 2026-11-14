//! Decoder robustness for the VP8 frame layer: malformed, truncated and bit-flipped key frames
//! must yield a typed error (or a valid frame), never a panic. The crate is
//! `#![forbid(unsafe_code)]`, so untrusted input cannot make decoding *memory*-unsafe; this pins
//! that no input drives it to *panic* either, which would be a denial of service on a server
//! decoding user uploads. The container-level half stays in `tests/robustness.rs`.

use gamut_color::{ChromaSubsampling, Planar8};
use gamut_core::Dimensions;

use super::{decode_frame, encode_frame};

/// A valid VP8 key-frame payload to mutate.
fn valid_payload() -> Vec<u8> {
    let yuv = Planar8::from_planes_subsampled(
        Dimensions::new(48, 32).unwrap(),
        ChromaSubsampling::Cs420,
        [
            (0..48 * 32).map(|i| (i % 251) as u8).collect(),
            vec![110u8; 24 * 16],
            vec![120u8; 24 * 16],
        ],
    )
    .unwrap();
    encode_frame(&yuv, 40)
        .expect("fixture fits the partition-size fields")
        .0
}

#[test]
fn vp8_decode_rejects_obviously_invalid_headers() {
    // Too short to hold the frame tag / uncompressed header, a zero header (bad start code),
    // and an all-ones header (the inverted key-frame bit marks it an unsupported interframe).
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
    // Truncating the valid payload at every length must never panic (it may error or decode
    // garbage).
    for len in 0..valid.len() {
        let _ = decode_frame(&valid[..len]);
    }
}

#[test]
fn vp8_decode_survives_bit_flips() {
    let valid = valid_payload();
    // Flip every bit of every byte except the 14-bit dimension fields (bytes 6..10, whose
    // corruption would only swap the valid frame for a differently-but-validly-sized one,
    // ballooning the test's memory). Corrupting the frame tag, start code, and the entire
    // entropy-coded body must not panic.
    for i in (0..valid.len()).filter(|&i| !(6..10).contains(&i)) {
        for bit in 0..8u8 {
            let mut bad = valid.clone();
            bad[i] ^= 1 << bit;
            let _ = decode_frame(&bad);
        }
    }
}
