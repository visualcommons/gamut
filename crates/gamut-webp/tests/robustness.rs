//! Decoder robustness corpus: malformed, truncated, and adversarial inputs must yield typed errors
//! (or a valid result), never a panic. gamut ships a `#![forbid(unsafe_code)]` decoder so decoding
//! untrusted input cannot be *memory*-unsafe; this corpus additionally pins that no input drives it
//! to *panic* (which would be a denial-of-service on a server decoding user uploads).
//!
//! This file holds the container-level half, through `WebpDecoder`. The VP8 frame-level half
//! (malformed headers, truncation, bit flips) drives the crate-private frame decoder and lives in
//! `src/vp8/frame/robustness_tests.rs`; the VP8L half is `tests/robustness_vp8l.rs`.

use gamut_core::{DecodeImage, ImageBuf, Rgb8};
use gamut_webp::WebpDecoder;

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
