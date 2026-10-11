//! `PngEncoder::with_transparent_cleanup` (issue #224): rewriting the colour of invisible pixels.
//!
//! The claim has two halves and they need different techniques. That nothing *visible* changes is
//! a differential claim, checked by decoding with libpng and comparing every pixel a viewer could
//! see. That it actually pays is a size claim, checked against the same image encoded without it.
//!
//! Both halves matter: a cleanup that changed a visible pixel would be a correctness bug, and one
//! that saved no bytes would be churn.

mod common;

use gamut_core::{Dimensions, EncodeImage, GrayAlpha16, ImageRef, Rgba8, Rgba16};
use gamut_png::{FilterStrategy, Level, PngEncoder};

const SIDE: u32 = 64;

fn encode(samples: &[u8], cleanup: bool, auto_reduce: bool) -> Vec<u8> {
    let dims = Dimensions::new(SIDE, SIDE).expect("valid dimensions");
    let image = ImageRef::<Rgba8>::new(samples, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_auto_reduce(auto_reduce)
        .with_transparent_cleanup(cleanup)
        .encode_image(image, &mut out)
        .expect("encode");
    out
}

#[test]
fn every_visible_pixel_survives_cleanup_unchanged() {
    // libpng decodes both files; every pixel with a non-zero alpha must be byte-identical, and
    // every alpha must be identical everywhere. Only the colour under alpha == 0 may differ.
    let src = common::corpus::sprite_rgba(SIDE);
    let plain = libpng_oracle::decode_rgba8(&encode(&src, false, false)).2;
    let cleaned = libpng_oracle::decode_rgba8(&encode(&src, true, false)).2;

    assert_eq!(plain.len(), cleaned.len());
    let mut invisible_changed = 0usize;
    let (plain_px, _) = plain.as_chunks::<4>();
    let (clean_px, _) = cleaned.as_chunks::<4>();
    for (i, (a, b)) in plain_px.iter().zip(clean_px).enumerate() {
        assert_eq!(a[3], b[3], "pixel {i}: alpha must never change");
        if a[3] == 0 {
            if a[..3] != b[..3] {
                invisible_changed += 1;
            }
        } else {
            assert_eq!(a, b, "pixel {i} is visible and must be byte-identical");
        }
    }
    assert!(
        invisible_changed > 0,
        "the fixture must actually exercise the cleanup"
    );
}

#[test]
fn cleanup_shrinks_an_image_with_invisible_colour_noise() {
    let src = common::corpus::sprite_rgba(SIDE);
    let plain = encode(&src, false, false);
    let cleaned = encode(&src, true, false);
    assert!(
        cleaned.len() < plain.len(),
        "cleanup should pay on a sprite: {} vs {}",
        cleaned.len(),
        plain.len()
    );
}

#[test]
fn cleanup_is_inert_on_a_fully_opaque_image() {
    // No fully transparent pixel means nothing to rewrite, and the output must be byte-identical
    // rather than merely the same size — this is what pins that the pass is a no-op, not a
    // re-encode that happens to land on the same length.
    let src = common::corpus::flat_rgba(SIDE);
    assert_eq!(encode(&src, false, true), encode(&src, true, true));
}

#[test]
fn cleanup_collapses_invisible_pixels_into_one_palette_entry() {
    // The compounding effect: `analyze8` keys its palette on the whole RGBA quad, so invisible
    // pixels that differ only in unseen colour cost an entry each. This fixture has 64 visible
    // colours and 64 *distinct* invisible ones, which is over the 256-entry cliff only in the
    // sense that it doubles the table; cleaning collapses the invisible half.
    let mut src = vec![0u8; (SIDE * SIDE * 4) as usize];
    for (i, px) in src.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let v = (i % 64) as u8;
        if i % 2 == 0 {
            px.copy_from_slice(&[v, v, v, 255]);
        } else {
            // Invisible, and every one a different colour.
            px.copy_from_slice(&[v.wrapping_mul(3), v.wrapping_add(7), 200 - v, 0]);
        }
    }
    let plain = encode(&src, false, true);
    let cleaned = encode(&src, true, true);
    assert!(
        cleaned.len() < plain.len(),
        "collapsing the invisible half should shrink the palette: {} vs {}",
        cleaned.len(),
        plain.len()
    );
}

#[test]
fn cleanup_is_off_by_default() {
    // The default must stay byte-for-byte lossless, so an encoder that was never asked for
    // cleanup must produce exactly what it produced before this feature existed.
    let src = common::corpus::sprite_rgba(SIDE);
    let dims = Dimensions::new(SIDE, SIDE).expect("valid dimensions");
    let image = ImageRef::<Rgba8>::new(&src, dims).expect("buffer matches dimensions");
    let mut default_out = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .encode_image(image, &mut default_out)
        .expect("encode");
    assert_eq!(default_out, encode(&src, false, false));
}

// --- 16-bit layouts -------------------------------------------------------------------------
//
// `Rgba16` and `GrayAlpha16` carry an alpha channel and can carry fully transparent pixels, so
// the knob's documented behaviour applies to them too. The oracle here is `libpng_oracle::decode`
// rather than `decode_rgba8`: the simplified reader would scale 16-bit samples down to 8 bits and
// hide exactly the low byte a byte-wise cleanup would get wrong.

/// A 16-bit sprite: an opaque disc over fully transparent pixels whose colour samples vary in
/// *both* bytes, so a cleanup that only cleared high bytes would leave compressible noise behind.
fn sprite_rgba16(side: u32) -> Vec<u16> {
    let mut buf = vec![0u16; (side * side * 4) as usize];
    let r2 = (i64::from(side) * i64::from(side)) / 9;
    for y in 0..side {
        for x in 0..side {
            let i = ((y * side + x) * 4) as usize;
            let cx = i64::from(x) - i64::from(side) / 2;
            let cy = i64::from(y) - i64::from(side) / 2;
            if cx * cx + cy * cy < r2 {
                buf[i] = u16::from((x ^ y) as u8) * 257;
                buf[i + 1] = 0x4040;
                buf[i + 2] = 0xC0C0;
                buf[i + 3] = u16::MAX;
            } else {
                // Invisible, and deliberately not constant in either byte of any sample.
                buf[i] = (x as u16).wrapping_mul(1103);
                buf[i + 1] = (y as u16).wrapping_mul(2749);
                buf[i + 2] = ((x ^ y) as u16).wrapping_mul(7919);
                buf[i + 3] = 0;
            }
        }
    }
    buf
}

/// The [`sprite_rgba16`] shape in two channels: an opaque grey band over invisible grey noise.
fn sprite_gray_alpha16(side: u32) -> Vec<u16> {
    let mut buf = vec![0u16; (side * side * 2) as usize];
    for y in 0..side {
        for x in 0..side {
            let i = ((y * side + x) * 2) as usize;
            if x % 8 < 5 {
                buf[i] = u16::from((y % 32) as u8) * 2048;
                buf[i + 1] = u16::MAX;
            } else {
                buf[i] = (x as u16).wrapping_mul(6151) ^ (y as u16).wrapping_mul(769);
                buf[i + 1] = 0;
            }
        }
    }
    buf
}

fn encode_rgba16(samples: &[u16], cleanup: bool) -> Vec<u8> {
    let dims = Dimensions::new(SIDE, SIDE).expect("valid dimensions");
    let image = ImageRef::<Rgba16>::new(samples, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_transparent_cleanup(cleanup)
        .encode_image(image, &mut out)
        .expect("encode");
    out
}

fn encode_gray_alpha16(samples: &[u16], cleanup: bool) -> Vec<u8> {
    let dims = Dimensions::new(SIDE, SIDE).expect("valid dimensions");
    let image = ImageRef::<GrayAlpha16>::new(samples, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_transparent_cleanup(cleanup)
        .encode_image(image, &mut out)
        .expect("encode");
    out
}

/// The decoded 16-bit samples, as big-endian pairs reassembled into `u16`.
fn decode16(png: &[u8], channels: usize) -> Vec<u16> {
    let decoded = libpng_oracle::decode(png);
    assert_eq!(decoded.bit_depth, 16, "the 16-bit path must stay 16-bit");
    assert_eq!(
        decoded.pixels.len(),
        (SIDE * SIDE) as usize * channels * 2,
        "unexpected layout"
    );
    decoded
        .pixels
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&p| u16::from_be_bytes(p))
        .collect()
}

#[test]
fn every_visible_rgba16_pixel_survives_cleanup_unchanged() {
    let src = sprite_rgba16(SIDE);
    let plain = decode16(&encode_rgba16(&src, false), 4);
    let cleaned = decode16(&encode_rgba16(&src, true), 4);

    let mut invisible_changed = 0usize;
    let (plain_px, _) = plain.as_chunks::<4>();
    let (clean_px, _) = cleaned.as_chunks::<4>();
    for (i, (a, b)) in plain_px.iter().zip(clean_px).enumerate() {
        assert_eq!(a[3], b[3], "pixel {i}: alpha must never change");
        if a[3] == 0 {
            assert_eq!(
                &b[..3],
                &[0, 0, 0],
                "pixel {i}: invisible colour must be zeroed"
            );
            if a[..3] != b[..3] {
                invisible_changed += 1;
            }
        } else {
            assert_eq!(a, b, "pixel {i} is visible and must be sample-identical");
        }
    }
    assert!(
        invisible_changed > 0,
        "the fixture must actually exercise the cleanup"
    );
}

#[test]
fn every_visible_gray_alpha16_pixel_survives_cleanup_unchanged() {
    let src = sprite_gray_alpha16(SIDE);
    let plain = decode16(&encode_gray_alpha16(&src, false), 2);
    let cleaned = decode16(&encode_gray_alpha16(&src, true), 2);

    let mut invisible_changed = 0usize;
    let (plain_px, _) = plain.as_chunks::<2>();
    let (clean_px, _) = cleaned.as_chunks::<2>();
    for (i, (a, b)) in plain_px.iter().zip(clean_px).enumerate() {
        assert_eq!(a[1], b[1], "pixel {i}: alpha must never change");
        if a[1] == 0 {
            assert_eq!(b[0], 0, "pixel {i}: invisible grey must be zeroed");
            if a[0] != b[0] {
                invisible_changed += 1;
            }
        } else {
            assert_eq!(a, b, "pixel {i} is visible and must be sample-identical");
        }
    }
    assert!(
        invisible_changed > 0,
        "the fixture must actually exercise the cleanup"
    );
}

#[test]
fn cleanup_shrinks_a_16_bit_image_with_invisible_noise() {
    // The knob's whole justification is that invisible noise costs real bytes, and it costs twice
    // as many of them per sample at 16 bits. Both fixtures carry it, so on both the cleaned
    // encoding must come out strictly smaller — the same claim
    // `cleanup_shrinks_an_image_with_invisible_colour_noise` makes for `Rgba8`.
    let rgba = sprite_rgba16(SIDE);
    assert!(
        encode_rgba16(&rgba, true).len() < encode_rgba16(&rgba, false).len(),
        "rgba16: {} vs {}",
        encode_rgba16(&rgba, true).len(),
        encode_rgba16(&rgba, false).len()
    );
    let grey = sprite_gray_alpha16(SIDE);
    assert!(
        encode_gray_alpha16(&grey, true).len() < encode_gray_alpha16(&grey, false).len(),
        "gray-alpha16: {} vs {}",
        encode_gray_alpha16(&grey, true).len(),
        encode_gray_alpha16(&grey, false).len()
    );
}

#[test]
fn cleanup_is_inert_on_a_fully_opaque_16_bit_image() {
    // No fully transparent pixel means the pass must not even copy the buffer: byte-identical
    // output, not merely equal length.
    let opaque: Vec<u16> = (0..(SIDE * SIDE))
        .flat_map(|i| [i as u16, 0x8686, 0xC1C1, u16::MAX])
        .collect();
    assert_eq!(
        encode_rgba16(&opaque, false),
        encode_rgba16(&opaque, true),
        "rgba16"
    );
}
