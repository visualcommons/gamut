//! `bKGD` and `sBIT` follow the colour type the encoder actually **writes**, not the one the
//! caller set them for (PNG §11.3.5.1, §11.3.3.4).
//!
//! Auto-reduce may write a different colour type from the input's — and since the palette and
//! colour-key candidates are *raced* against the unreduced encoding, which one lands is decided by
//! compressed size, not by anything the caller can predict when it calls `with_background_index`
//! or `with_significant_bits`. A `bKGD`/`sBIT` payload shaped for the wrong colour type is a chunk
//! libpng rejects (`pngrutil.c`, `png_handle_bKGD` / `png_handle_sBIT`: the length must match the
//! colour type, an index must be inside the palette, every value must fit the bit depth) and
//! silently drops. The encoder therefore converts each to the written header where a lossless
//! conversion exists — RGBA `sBIT` loses only its alpha entry, an RGB background becomes the index
//! of that palette entry, a grey RGB triple collapses to one grey sample — and omits the chunk
//! otherwise.
//!
//! **Technique: exact-byte over the emitted chunk stream, against libpng's own acceptance rules,
//! plus a libpng decode of every file.** The vendored oracle exposes neither `bKGD`/`sBIT` nor a
//! warning count (its warning callback discards benign errors), so libpng's acceptance of the
//! *chunk* is not observable through it today; the assertion is on the payload libpng's rules
//! accept for the written IHDR, and the decode proves the file around it is sound.

mod common;

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut_png::PngEncoder;
use libpng_oracle::{COLOR_GRAY, COLOR_PALETTE, COLOR_RGB, COLOR_RGBA};

/// The payload of the first chunk of type `want`, or `None` if the file carries none.
fn read_chunk(png: &[u8], want: &[u8; 4]) -> Option<Vec<u8>> {
    let mut at = 8; // signature
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
        if &png[at + 4..at + 8] == want {
            return Some(png[at + 8..at + 8 + len].to_vec());
        }
        at += 12 + len;
    }
    None
}

/// Auto-reduce on, everything else default: the palette and colour-key races both run.
fn encoder() -> PngEncoder {
    PngEncoder::new().with_auto_reduce(true)
}

fn encode_rgba(encoder: &PngEncoder, side: u32, samples: &[u8]) -> Vec<u8> {
    let dims = Dimensions::new(side, side).expect("valid dimensions");
    let image = ImageRef::<Rgba8>::new(samples, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    encoder.encode_image(image, &mut out).expect("encode");
    out
}

fn encode_rgb(encoder: &PngEncoder, side: u32, samples: &[u8]) -> Vec<u8> {
    let dims = Dimensions::new(side, side).expect("valid dimensions");
    let image = ImageRef::<Rgb8>::new(samples, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    encoder.encode_image(image, &mut out).expect("encode");
    out
}

/// The colour type libpng reads from the file — the one the race chose. Reading it through the
/// oracle also proves the file around the chunk under test is one libpng decodes.
fn written_colour_type(png: &[u8]) -> u8 {
    libpng_oracle::decode(png).color_type
}

/// Two opaque, non-grey colours in a checkerboard: a one-bit palette wins by a mile.
const INK: [u8; 4] = [200, 30, 60, 255];
const PAPER: [u8; 4] = [20, 90, 220, 255];

fn two_colour_rgba(side: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            buf.extend_from_slice(if (x + y) % 2 == 0 { &INK } else { &PAPER });
        }
    }
    buf
}

/// Binary alpha over one shared invisible colour, with too many visible colours for a palette:
/// the `tRNS` colour key is the only reduction on the table, and at 128 it wins (see
/// `tests/colour_key.rs`, which measured the crossover).
fn keyable_rgba(side: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let cx = i64::from(x) - i64::from(side) / 2;
            let cy = i64::from(y) - i64::from(side) / 2;
            if cx * cx + cy * cy >= (i64::from(side) * i64::from(side)) / 9 {
                buf.extend_from_slice(&[1, 2, 3, 0]);
            } else {
                buf.extend_from_slice(&[(x * 2) as u8, (y * 2) as u8, 200, 255]);
            }
        }
    }
    buf
}

/// A sprite whose invisible pixels carry noise until cleanup zeroes them to `(0, 0, 0, 0)`, with
/// opaque black among its three visible colours. After cleanup the derived palette holds **two**
/// entries with the triple `[0, 0, 0]` — the transparent one first, by the encoder's
/// transparent-first ordering — so a black background has to choose between them.
fn black_on_transparent_rgba(side: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let cx = i64::from(x) - i64::from(side) / 2;
            let cy = i64::from(y) - i64::from(side) / 2;
            if cx * cx + cy * cy >= (i64::from(side) * i64::from(side)) / 9 {
                // Invisible noise: an avalanche hash of the position, so that plain RGBA cannot
                // compress it and cleanup is what makes the palette reachable.
                let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B))
                    .wrapping_mul(0x27D4_EB2F);
                let [a, b, c, _] = h.to_be_bytes();
                buf.extend_from_slice(&[a, b, c, 0]);
            } else {
                // Visible pixels pick one of three colours pseudo-randomly, so that the palette
                // (two bits per pixel) beats plain RGBA (four bytes per pixel) on real bytes
                // rather than losing the race to a stripe pattern DEFLATE matches for free.
                let h = (x.wrapping_mul(0x1656_67B1) ^ y.wrapping_mul(0xC2B2_AE35))
                    .wrapping_mul(0x9E37_79B9);
                buf.extend_from_slice(match (h >> 24) % 3 {
                    0 => &[0, 0, 0, 255],
                    1 => &INK,
                    _ => &PAPER,
                });
            }
        }
    }
    buf
}

/// The alpha of palette entry `index` — 255 past the end of `tRNS` (§11.3.2.1).
fn palette_alpha(trns: Option<&[u8]>, index: usize) -> u8 {
    trns.and_then(|t| t.get(index).copied()).unwrap_or(255)
}

#[test]
fn an_rgb_background_names_the_opaque_entry_not_the_transparent_twin() {
    let src = black_on_transparent_rgba(64);
    let png = encode_rgba(
        &encoder()
            .with_transparent_cleanup(true)
            .with_background_rgb(0, 0, 0),
        64,
        &src,
    );

    assert_eq!(
        written_colour_type(&png),
        COLOR_PALETTE,
        "precondition: the palette won"
    );
    let plte = read_chunk(&png, b"PLTE").expect("an indexed file carries PLTE");
    let trns = read_chunk(&png, b"tRNS");
    let blacks: Vec<usize> = plte
        .as_chunks::<3>()
        .0
        .iter()
        .enumerate()
        .filter(|(_, entry)| **entry == [0, 0, 0])
        .map(|(i, _)| i)
        .collect();
    let transparent = blacks
        .iter()
        .copied()
        .find(|&i| palette_alpha(trns.as_deref(), i) == 0)
        .expect("precondition: cleanup left a transparent black entry");
    let opaque = blacks
        .iter()
        .copied()
        .find(|&i| palette_alpha(trns.as_deref(), i) == 255)
        .expect("precondition: the visible black is an opaque entry");
    assert!(
        transparent < opaque,
        "precondition: the transparent twin comes first, so a first-match search would pick it"
    );
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        Some(vec![opaque as u8]),
        "the background is a colour a viewer sees: the opaque entry, not its transparent twin"
    );
}

#[test]
fn a_palette_index_background_is_dropped_under_an_encoder_derived_palette() {
    // The palette wins here, but it is the encoder's palette, in the encoder's order: the
    // caller's index names an entry in a palette the caller never saw.
    let src = two_colour_rgba(64);
    let png = encode_rgba(&encoder().with_background_index(1), 64, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_PALETTE,
        "precondition: the palette won"
    );
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        None,
        "an index into a palette the caller did not supply refers to nothing"
    );
}

#[test]
fn a_palette_index_background_is_dropped_when_the_unreduced_stream_wins() {
    // 64 colours at 32x32: the palette's flat PLTE+tRNS bytes are not amortised, so the unreduced
    // RGBA stream wins the race (STATUS.md's cost-model table) and the caller's index has no
    // palette to point into.
    let src = common::corpus::palette64_rgba(32);
    let png = encode_rgba(&encoder().with_background_index(0), 32, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_RGBA,
        "precondition: the unreduced stream won"
    );
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        None,
        "a one-byte palette index under colour type 6 is a chunk libpng drops"
    );
}

#[test]
fn rgba_significant_bits_lose_their_alpha_entry_under_a_colour_key() {
    let src = keyable_rgba(128);
    let png = encode_rgba(&encoder().with_significant_bits(&[8, 8, 8, 8]), 128, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_RGB,
        "precondition: the colour key dropped the alpha channel"
    );
    assert_eq!(
        read_chunk(&png, b"sBIT"),
        Some(vec![8, 8, 8]),
        "three entries for truecolour: the alpha entry describes a channel that is gone"
    );
}

#[test]
fn an_rgb_background_becomes_that_entrys_index_when_the_palette_wins() {
    let src = two_colour_rgba(64);
    let (r, g, b) = (PAPER[0], PAPER[1], PAPER[2]);
    let png = encode_rgba(
        &encoder().with_background_rgb(r.into(), g.into(), b.into()),
        64,
        &src,
    );

    assert_eq!(
        written_colour_type(&png),
        COLOR_PALETTE,
        "precondition: the palette won"
    );
    let plte = read_chunk(&png, b"PLTE").expect("an indexed file carries PLTE");
    let index = plte
        .as_chunks::<3>()
        .0
        .iter()
        .position(|entry| *entry == [r, g, b])
        .expect("the background colour is a palette entry");
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        Some(vec![index as u8]),
        "one byte: the index of the entry holding the caller's colour"
    );
}

#[test]
fn rgba_significant_bits_become_three_under_a_palette() {
    let src = two_colour_rgba(64);
    let png = encode_rgba(&encoder().with_significant_bits(&[8, 8, 8, 8]), 64, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_PALETTE,
        "precondition: the palette won"
    );
    assert_eq!(
        read_chunk(&png, b"sBIT"),
        Some(vec![8, 8, 8]),
        "an indexed sBIT is always three entries, whatever the index depth (§11.3.3.4)"
    );
}

#[test]
fn a_grey_rgb_background_collapses_to_one_sample_under_greyscale() {
    let src = common::corpus::grey_as_rgb(32);
    let png = encode_rgb(&encoder().with_background_rgb(77, 77, 77), 32, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_GRAY,
        "precondition: the RGB input reduced to greyscale"
    );
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        Some(vec![0, 77]),
        "one 16-bit big-endian grey sample"
    );
}

#[test]
fn a_coloured_background_has_no_greyscale_form_and_is_dropped() {
    let src = common::corpus::grey_as_rgb(32);
    let png = encode_rgb(&encoder().with_background_rgb(1, 2, 3), 32, &src);

    assert_eq!(
        written_colour_type(&png),
        COLOR_GRAY,
        "precondition: the RGB input reduced to greyscale"
    );
    assert_eq!(
        read_chunk(&png, b"bKGD"),
        None,
        "a background no greyscale sample can name is omitted rather than written wrong"
    );
}

#[test]
fn chunks_set_for_the_written_colour_type_pass_through_unchanged() {
    // The control: an RGBA image that stays RGBA (partial alpha, many colours) keeps its
    // four-entry sBIT and six-byte bKGD byte for byte, so the conversion is inert where nothing
    // changed.
    let side = 16u32;
    let src: Vec<u8> = (0..side * side)
        .flat_map(|i| {
            [
                (i * 7) as u8,
                (i * 13) as u8,
                (i * 29) as u8,
                (i % 7 * 40) as u8,
            ]
        })
        .collect();
    let png = encode_rgba(
        &encoder()
            .with_significant_bits(&[5, 6, 5, 4])
            .with_background_rgb(1, 2, 3),
        side,
        &src,
    );

    assert_eq!(
        written_colour_type(&png),
        COLOR_RGBA,
        "precondition: nothing reduced"
    );
    assert_eq!(read_chunk(&png, b"sBIT"), Some(vec![5, 6, 5, 4]));
    assert_eq!(read_chunk(&png, b"bKGD"), Some(vec![0, 1, 0, 2, 0, 3]));
}
