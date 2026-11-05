//! VP8L (lossless) container robustness: hostile `VP8L` bodies wrapped in a valid RIFF/WebP
//! container, and truncations of a whole file, must yield typed errors (or a valid result) from the
//! public decoder, never a panic. The codestream-level corpus — bare payloads straight into the
//! VP8L decoder, and the headerless `ALPH` path — needs the crate's private decoder and lives
//! beside it (`src/vp8l/decoder.rs`, `src/alpha.rs`).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
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
fn webp_container_survives_corrupted_vp8l_chunk() {
    // Hostile bodies wrapped in a *valid* RIFF/WebP container and run through the public decoder:
    // container parsing plus VP8L decode together must never panic on a corrupt payload.
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
