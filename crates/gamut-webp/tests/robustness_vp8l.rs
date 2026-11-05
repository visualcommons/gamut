//! VP8L (lossless) decoder robustness corpus: malformed, truncated, and adversarial inputs must
//! yield typed errors (or a valid result), never a panic. The existing `robustness.rs` covers the VP8
//! *lossy* path; this file gives the **lossless** decoder the same hostile-input pinning. That matters
//! more here, not less: lossless is the path that runs on every PNG-style upload and on compressed
//! (`C=1`) alpha, and the memory-unsafety of the C reference (CVE-2023-4863) is the whole reason gamut
//! ships a `#![forbid(unsafe_code)]` decoder — which is moot if hostile input can still *panic* it
//! into a denial of service.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_riff::{RiffReader, WebpChunkId};
use gamut_webp::vp8l::decoder::decode as decode_vp8l;
use gamut_webp::{WebpDecoder, WebpEncoder};

/// A small but non-trivial RGB image (gradients + a bit of structure) so the encoded VP8L stream uses
/// transforms, LZ77 back-references, and the color cache — the parts a fuzzer should reach.
fn sample_rgb(w: u32, h: u32) -> Vec<u8> {
    (0..(w * h) as usize)
        .flat_map(|i| {
            let (x, y) = (i as u32 % w, i as u32 / w);
            [
                ((x * 7 + y * 3) & 0xff) as u8,
                ((x ^ (y * 5)) & 0xff) as u8,
                ((x * x + y) & 0xff) as u8,
            ]
        })
        .collect()
}

/// Encodes a small image losslessly and returns the bare `VP8L` chunk payload (the bitstream the
/// native decoder consumes), for mutation by the corpus below.
fn valid_vp8l_payload() -> Vec<u8> {
    let (w, h) = (24u32, 18u32);
    let mut file = Vec::new();
    WebpEncoder::lossless()
        .encode_image(
            ImageRef::<Rgb8>::new(&sample_rgb(w, h), Dimensions::new(w, h).unwrap()).unwrap(),
            &mut file,
        )
        .expect("gamut lossless encode");
    RiffReader::new(&file)
        .expect("riff")
        .filter_map(Result::ok)
        .find(|c| matches!(WebpChunkId::from(c.fourcc), WebpChunkId::Vp8l))
        .expect("VP8L chunk")
        .payload
        .to_vec()
}

/// A cheap deterministic byte generator (an LCG), so the fuzz corpus is reproducible and
/// version-controlled rather than depending on a random source.
fn pseudo_random_bytes(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed ^ 0x9e37_79b9;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn vp8l_decode_rejects_obviously_invalid_headers() {
    // Empty, too short to hold the 0x2f signature + 28-bit dimension/alpha/version word, a wrong
    // signature byte, and all-ones must all be rejected (never panic). 0x2f is the VP8L signature.
    for bad in [
        &[][..],
        &[0x2f][..],
        &[0x2f, 0x00][..],
        &[0x00, 0x00, 0x00, 0x00, 0x00][..],
        &[0xff, 0xff, 0xff, 0xff, 0xff][..],
    ] {
        assert!(
            decode_vp8l(bad).is_err(),
            "{}-byte garbage must be rejected",
            bad.len()
        );
    }
}

#[test]
fn vp8l_decode_survives_truncation() {
    let valid = valid_vp8l_payload();
    assert!(decode_vp8l(&valid).is_ok(), "the baseline payload decodes");
    // Decoding every length-prefix must never panic (it may error or decode a partial image). The
    // image is small, so the in-header dimensions can only shrink coverage, never balloon allocation.
    for len in 0..valid.len() {
        let _ = decode_vp8l(&valid[..len]);
    }
}

#[test]
fn vp8l_decode_survives_bit_flips() {
    let valid = valid_vp8l_payload();
    // Flip every bit of every byte except the 14-bit width / 14-bit height fields (the 28-bit word
    // after the 0x2f signature, i.e. bytes 1..5), whose corruption would only swap the image for a
    // differently-but-validly-sized one and could balloon the decode allocation. Corrupting the
    // signature and the entire entropy-coded body (prefix codes, transforms, LZ77, color cache) must
    // not panic.
    for i in (0..valid.len()).filter(|&i| !(1..5).contains(&i)) {
        for bit in 0..8u8 {
            let mut bad = valid.clone();
            bad[i] ^= 1 << bit;
            let _ = decode_vp8l(&bad);
        }
    }
}

#[test]
fn vp8l_decode_survives_fuzzed_bodies() {
    // Keep the valid header (so the fuzzer gets past dimension parsing into the interesting transform
    // / prefix-code / LZ77 / color-cache machinery on a bounded-size image) and replace the body with
    // deterministic pseudo-random bytes of varied lengths. None may panic.
    let valid = valid_vp8l_payload();
    let head = &valid[..valid.len().min(5)]; // 0x2f signature + dimension/alpha/version word
    for seed in 0..256u32 {
        for &len in &[0usize, 1, 4, 16, 64, 256, 1024] {
            let mut buf = head.to_vec();
            buf.extend(pseudo_random_bytes(seed, len));
            let _ = decode_vp8l(&buf);
        }
    }
}

#[test]
fn webp_container_survives_corrupted_vp8l_chunk() {
    // The same hostile bodies wrapped in a *valid* RIFF/WebP container and run through the public
    // decoder: container parsing plus VP8L decode together must never panic on a corrupt payload.
    let (w, h) = (24u32, 18u32);
    let mut file = Vec::new();
    WebpEncoder::lossless()
        .encode_image(
            ImageRef::<Rgb8>::new(&sample_rgb(w, h), Dimensions::new(w, h).unwrap()).unwrap(),
            &mut file,
        )
        .expect("encode");
    // A simple-lossless file is RIFF(12) + "VP8L"(4) + chunk-size(4) + payload, so the bitstream
    // starts at offset 20; corrupting from there leaves the container parseable but the body garbage.
    let payload_start = 20.min(file.len());
    let dec = WebpDecoder::new();
    for seed in 0..64u32 {
        let mut bad = file.clone();
        let noise = pseudo_random_bytes(seed, bad.len() - payload_start);
        bad[payload_start..].copy_from_slice(&noise);
        let _: gamut_core::Result<ImageBuf<Rgb8>> = dec.decode_image(&bad); // err or ok, never panic
    }
    // Truncating the whole file at every length must also be panic-free.
    for cut in 0..file.len() {
        let _: gamut_core::Result<ImageBuf<Rgb8>> = dec.decode_image(&file[..cut]);
    }
}

#[test]
fn alph_lossless_decode_survives_malformed() {
    // The headerless VP8L path that backs compressed (`C=1`) `ALPH` alpha: `read_alph` must survive a
    // garbage payload (advertising compression, then random bytes) without panicking, for a range of
    // declared dimensions.
    for &(w, h) in &[(1usize, 1usize), (16, 16), (24, 18), (33, 7)] {
        for seed in 0..128u32 {
            // Fuzz the ALPH header byte (covers raw `C=0`, VP8L `C=1`, and the reserved methods, plus
            // every filter) followed by random body bytes — the C=1 path runs the headerless VP8L
            // decoder on arbitrary prefix/transform/LZ77 data, which must error, not panic.
            let mut payload = vec![(seed & 0xff) as u8];
            payload.extend(pseudo_random_bytes(seed ^ 0x1234, 48));
            let _ = gamut_webp::alpha::read_alph(&payload, w, h);
        }
    }
}
