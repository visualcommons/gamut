//! Differential decoder conformance against a vendored **libpng**.
//!
//! Every fixture is *generated* by libpng's reference encoder (`libpng_oracle::encode`) — no
//! vendored corpus — then decoded by both gamut-png and libpng, asserting header agreement and
//! sample-exact pixels. Interlaced, sub-byte, forced-filter, and ancillary-laden streams that
//! the gamut encoder cannot produce are exactly the point of this suite.

mod common;

use common::{channels, gray8_scale, sample_bytes, tiny_exif, tiny_icc_profile};
use gamut_core::{DecodeImage, GrayAlpha8, ImageBuf, Rgba8, Rgba16};
use gamut_png::{PngDecoder, PngImage};
use libpng_oracle::{EncodeOpts, TextChunk, TextKind};

/// Encodes a deterministic fixture with libpng (full-size palette for indexed depths).
fn fixture(
    width: u32,
    height: u32,
    color_type: u8,
    depth: u8,
    interlace: bool,
    seed: u32,
) -> Vec<u8> {
    let pixels = sample_bytes(width, height, color_type, depth, seed);
    if color_type == libpng_oracle::COLOR_PALETTE {
        let entries = 1usize << depth;
        let palette: Vec<[u8; 3]> = (0..entries)
            .map(|i| [i as u8, (i * 7 + 3) as u8, 255 - i as u8])
            .collect();
        libpng_oracle::encode(
            &pixels,
            width,
            height,
            color_type,
            depth,
            &EncodeOpts {
                interlace,
                palette: Some(&palette),
                ..EncodeOpts::default()
            },
        )
    } else {
        libpng_oracle::encode(
            &pixels,
            width,
            height,
            color_type,
            depth,
            &EncodeOpts {
                interlace,
                ..EncodeOpts::default()
            },
        )
    }
}

/// Lane A: gamut's rich decode against libpng's transform-free decode — header fields and
/// native samples must agree byte for byte (sub-byte grey compares after the §13.12 scaling
/// gamut applies and the oracle does not; indices are unscaled on both sides).
fn assert_matches_oracle(png: &[u8], context: &str) {
    let oracle = libpng_oracle::decode(png);
    let decoded = PngDecoder::new()
        .decode(png)
        .unwrap_or_else(|e| panic!("{context}: gamut decode failed: {e}"));
    assert_eq!(
        (decoded.header.width, decoded.header.height),
        (oracle.width, oracle.height),
        "{context}: dimensions"
    );
    assert_eq!(
        decoded.header.bit_depth, oracle.bit_depth,
        "{context}: bit depth"
    );
    assert_eq!(
        decoded.header.color_type.code(),
        oracle.color_type,
        "{context}: colour type"
    );
    assert_eq!(
        decoded.header.interlaced, oracle.interlace,
        "{context}: interlace"
    );

    let expected: Vec<u8> =
        if oracle.color_type == libpng_oracle::COLOR_GRAY && oracle.bit_depth < 8 {
            let scale = gray8_scale(oracle.bit_depth);
            oracle.pixels.iter().map(|&v| v * scale).collect()
        } else {
            oracle.pixels.clone()
        };
    let actual: Vec<u8> = match &decoded.image {
        PngImage::Gray8(img) => img.as_samples().to_vec(),
        PngImage::GrayAlpha8(img) => img.as_samples().to_vec(),
        PngImage::Rgb8(img) => img.as_samples().to_vec(),
        PngImage::Rgba8(img) => img.as_samples().to_vec(),
        PngImage::Indexed8(img) => img.as_samples().to_vec(),
        PngImage::Gray16(img) => be_bytes(img.as_samples()),
        PngImage::GrayAlpha16(img) => be_bytes(img.as_samples()),
        PngImage::Rgb16(img) => be_bytes(img.as_samples()),
        PngImage::Rgba16(img) => be_bytes(img.as_samples()),
    };
    assert_eq!(actual, expected, "{context}: samples");
}

/// Lane B (8-bit-or-below files only): gamut's typed RGBA widening against libpng's simplified
/// RGBA read, which resolves palettes, tRNS, and sub-byte grey the same way.
fn assert_rgba8_matches_oracle(png: &[u8], context: &str) {
    let (width, height, expected) = libpng_oracle::decode_rgba8(png);
    let img: ImageBuf<Rgba8> = PngDecoder::new()
        .decode_image(png)
        .unwrap_or_else(|e| panic!("{context}: gamut rgba8 decode failed: {e}"));
    assert_eq!(
        (img.width(), img.height()),
        (width, height),
        "{context}: rgba8 dims"
    );
    assert_eq!(img.as_samples(), expected, "{context}: rgba8 samples");
}

fn be_bytes(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_be_bytes()).collect()
}

/// Every valid Table-12 colour-type/bit-depth combination, both interlace modes, at a size
/// that is byte-unaligned for the sub-byte depths and hits partial Adam7 passes.
#[test]
fn table12_grid_matches_oracle() {
    let combos: &[(u8, &[u8])] = &[
        (libpng_oracle::COLOR_GRAY, &[1, 2, 4, 8, 16]),
        (libpng_oracle::COLOR_PALETTE, &[1, 2, 4, 8]),
        (libpng_oracle::COLOR_RGB, &[8, 16]),
        (libpng_oracle::COLOR_GRAY_ALPHA, &[8, 16]),
        (libpng_oracle::COLOR_RGBA, &[8, 16]),
    ];
    for &(color_type, depths) in combos {
        for &depth in depths {
            for interlace in [false, true] {
                let png = fixture(17, 13, color_type, depth, interlace, 11);
                let context = format!("ct{color_type}/d{depth}/adam7={interlace}");
                assert_matches_oracle(&png, &context);
                if depth <= 8 {
                    assert_rgba8_matches_oracle(&png, &context);
                }
            }
        }
    }
}

/// The Adam7 stress block: every size in 1..=9 × 1..=9 (all empty-pass and partial-byte
/// combinations) across four representative layouts, plus larger spot checks.
#[test]
fn adam7_exhaustive_small_sizes_match_oracle() {
    let layouts = [
        (libpng_oracle::COLOR_RGB, 8u8),
        (libpng_oracle::COLOR_GRAY, 1),
        (libpng_oracle::COLOR_PALETTE, 4),
        (libpng_oracle::COLOR_RGBA, 16),
    ];
    for (color_type, depth) in layouts {
        for width in 1..=9u32 {
            for height in 1..=9u32 {
                let png = fixture(width, height, color_type, depth, true, width * 100 + height);
                assert_matches_oracle(&png, &format!("ct{color_type}/d{depth} {width}x{height}"));
            }
        }
        for (width, height) in [(16, 16), (33, 5), (5, 33), (40, 40)] {
            let png = fixture(width, height, color_type, depth, true, width + height);
            assert_matches_oracle(&png, &format!("ct{color_type}/d{depth} {width}x{height}"));
        }
    }
}

/// Forces each filter type (and the free choice) through libpng on smooth gradients, where
/// filter reconstruction errors cannot hide, including the zero-predecessor edge rows/columns.
#[test]
fn forced_filters_match_oracle() {
    let masks = [
        ("none", libpng_oracle::FILTER_NONE),
        ("sub", libpng_oracle::FILTER_SUB),
        ("up", libpng_oracle::FILTER_UP),
        ("avg", libpng_oracle::FILTER_AVG),
        ("paeth", libpng_oracle::FILTER_PAETH),
        ("all", libpng_oracle::FILTER_ALL),
    ];
    let layouts = [
        (libpng_oracle::COLOR_RGB, 8u8),
        (libpng_oracle::COLOR_GRAY, 16),
        (libpng_oracle::COLOR_GRAY_ALPHA, 8),
    ];
    for (color_type, depth) in layouts {
        for &(name, mask) in &masks {
            for interlace in [false, true] {
                for (width, height) in [(1, 8), (8, 1), (24, 17)] {
                    let pixels = gradient(width, height, color_type, depth);
                    let png = libpng_oracle::encode(
                        &pixels,
                        width,
                        height,
                        color_type,
                        depth,
                        &EncodeOpts {
                            interlace,
                            filters: Some(mask),
                            ..EncodeOpts::default()
                        },
                    );
                    let context = format!(
                        "ct{color_type}/d{depth}/{name}/adam7={interlace}/{width}x{height}"
                    );
                    assert_matches_oracle(&png, &context);
                }
            }
        }
    }
}

/// A smooth per-channel gradient (filters produce small residuals, so a wrong predictor is
/// glaring), in the oracle's byte-per-sample layout.
fn gradient(width: u32, height: u32, color_type: u8, depth: u8) -> Vec<u8> {
    let channels = channels(color_type);
    let bytes_per_sample = if depth == 16 { 2 } else { 1 };
    let mut out = Vec::with_capacity((width * height) as usize * channels * bytes_per_sample);
    for y in 0..height {
        for x in 0..width {
            for c in 0..channels {
                let value = (x * 3 + y * 5 + c as u32 * 11) as u8;
                if depth == 16 {
                    out.extend_from_slice(&[value, value.wrapping_mul(7)]);
                } else {
                    out.push(value);
                }
            }
        }
    }
    out
}

/// Palette geometry: sizes crossing every bit-depth boundary, with absent, partial, and
/// full-length tRNS.
#[test]
fn palette_geometry_matches_oracle() {
    for entries in [1usize, 2, 3, 4, 5, 16, 17, 255, 256] {
        let depth = match entries {
            0..=2 => 1u8,
            3..=4 => 2,
            5..=16 => 4,
            _ => 8,
        };
        let palette: Vec<[u8; 3]> = (0..entries)
            .map(|i| [(i * 5) as u8, (255 - i) as u8, (i * 13 + 1) as u8])
            .collect();
        let (width, height) = (19u32, 7u32);
        let indices: Vec<u8> = (0..(width * height) as usize)
            .map(|i| (i % entries) as u8)
            .collect();
        let trns_full: Vec<u8> = (0..entries).map(|i| (i * 37) as u8).collect();
        let trns_partial = &trns_full[..entries / 2];
        let cases: [(&str, Option<&[u8]>); 3] = [
            ("opaque", None),
            ("partial-trns", Some(trns_partial)),
            ("full-trns", Some(&trns_full)),
        ];
        for (name, trns) in cases {
            if trns.is_some_and(<[u8]>::is_empty) {
                continue; // a zero-length tRNS is not a meaningful fixture
            }
            let png = libpng_oracle::encode(
                &indices,
                width,
                height,
                libpng_oracle::COLOR_PALETTE,
                depth,
                &EncodeOpts {
                    palette: Some(&palette),
                    trns_palette: trns,
                    ..EncodeOpts::default()
                },
            );
            let context = format!("{entries} entries/{name}");
            assert_matches_oracle(&png, &context);
            assert_rgba8_matches_oracle(&png, &context);
        }
    }
}

/// tRNS colour keys on greyscale and truecolour files: the keyed pixels (and only those) must
/// resolve to alpha zero.
#[test]
fn colour_key_transparency_matches_oracle() {
    // 8-bit grey and RGB: differential against libpng's RGBA resolution.
    for color_type in [libpng_oracle::COLOR_GRAY, libpng_oracle::COLOR_RGB] {
        let (width, height) = (11u32, 6u32);
        let pixels = sample_bytes(width, height, color_type, 8, 5);
        let opts = if color_type == libpng_oracle::COLOR_GRAY {
            EncodeOpts {
                trns_gray: Some(u16::from(pixels[0])),
                ..EncodeOpts::default()
            }
        } else {
            EncodeOpts {
                trns_rgb: Some([
                    u16::from(pixels[0]),
                    u16::from(pixels[1]),
                    u16::from(pixels[2]),
                ]),
                ..EncodeOpts::default()
            }
        };
        let png = libpng_oracle::encode(&pixels, width, height, color_type, 8, &opts);
        let context = format!("key ct{color_type}/d8");
        assert_matches_oracle(&png, &context);
        assert_rgba8_matches_oracle(&png, &context);
    }

    // Sub-byte greyscale with a colour key exercises the one path that both scales a sample to
    // 8 bits (§13.12) and derives an alpha channel from the key -- the keyed value must land on
    // `scale * key` with alpha 0, and every other value on `scale * value` with alpha 255.
    for bit_depth in [1u8, 2, 4] {
        let (width, height) = (13u32, 5u32);
        let pixels = sample_bytes(width, height, libpng_oracle::COLOR_GRAY, bit_depth, 9);
        let opts = EncodeOpts {
            trns_gray: Some(u16::from(pixels[0])),
            ..EncodeOpts::default()
        };
        let png = libpng_oracle::encode(
            &pixels,
            width,
            height,
            libpng_oracle::COLOR_GRAY,
            bit_depth,
            &opts,
        );
        let context = format!("key ct0/d{bit_depth}");
        assert_rgba8_matches_oracle(&png, &context);

        // Assert the scaling directly too: libpng agreeing is necessary but does not pin which
        // arithmetic produced it, and a divide-instead-of-multiply is invisible at bit depth 8.
        let scale = gray8_scale(bit_depth);
        let img: ImageBuf<GrayAlpha8> = PngDecoder::new()
            .decode_image(&png)
            .unwrap_or_else(|e| panic!("{context}: grey+alpha decode failed: {e}"));
        for (out, &raw) in img
            .as_samples()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(pixels.iter())
        {
            assert_eq!(out[0], raw * scale, "{context}: sample {raw} scaled wrong");
            let opaque = u16::from(raw) != u16::from(pixels[0]);
            assert_eq!(out[1], u8::from(opaque) * 255, "{context}: alpha for {raw}");
        }
    }

    // 16-bit: libpng's simplified RGBA read is 8-bit, so assert the keying directly.
    let (width, height) = (9u32, 4u32);
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_RGB, 16, 21);
    let key = [
        u16::from_be_bytes([pixels[0], pixels[1]]),
        u16::from_be_bytes([pixels[2], pixels[3]]),
        u16::from_be_bytes([pixels[4], pixels[5]]),
    ];
    let png = libpng_oracle::encode(
        &pixels,
        width,
        height,
        libpng_oracle::COLOR_RGB,
        16,
        &EncodeOpts {
            trns_rgb: Some(key),
            ..EncodeOpts::default()
        },
    );
    assert_matches_oracle(&png, "key rgb16");
    let img: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
    let mut keyed = 0;
    for (px, src) in img
        .as_samples()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(pixels.as_chunks::<6>().0)
    {
        let native = [
            u16::from_be_bytes([src[0], src[1]]),
            u16::from_be_bytes([src[2], src[3]]),
            u16::from_be_bytes([src[4], src[5]]),
        ];
        assert_eq!(&px[..3], native, "rgb16 colour survives");
        let expected_alpha = if native == key { 0 } else { u16::MAX };
        assert_eq!(px[3], expected_alpha, "rgb16 key alpha");
        keyed += usize::from(native == key);
    }
    assert!(keyed >= 1, "the key must match at least the first pixel");

    // 16-bit grey: the key resolves through the GrayAlpha16 widening.
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_GRAY, 16, 22);
    let key = u16::from_be_bytes([pixels[0], pixels[1]]);
    let png = libpng_oracle::encode(
        &pixels,
        width,
        height,
        libpng_oracle::COLOR_GRAY,
        16,
        &EncodeOpts {
            trns_gray: Some(key),
            ..EncodeOpts::default()
        },
    );
    assert_matches_oracle(&png, "key gray16");
    let img: ImageBuf<gamut_core::GrayAlpha16> = PngDecoder::new().decode_image(&png).unwrap();
    for (px, src) in img
        .as_samples()
        .as_chunks::<2>()
        .0
        .iter()
        .zip(pixels.as_chunks::<2>().0)
    {
        let native = u16::from_be_bytes([src[0], src[1]]);
        assert_eq!(px[0], native);
        assert_eq!(px[1], if native == key { 0 } else { u16::MAX });
    }
}

/// 16-bit byte order: asymmetric hi/lo byte patterns land as the same u16 values libpng reads.
#[test]
fn sixteen_bit_endianness_matches_oracle() {
    let (width, height) = (5u32, 3u32);
    let pixels: Vec<u8> = (0..(width * height * 3) as usize)
        .flat_map(|i| [0x01 + (i % 3) as u8, 0xFE - (i % 5) as u8])
        .collect();
    let png = libpng_oracle::encode(
        &pixels,
        width,
        height,
        libpng_oracle::COLOR_RGB,
        16,
        &EncodeOpts::default(),
    );
    assert_matches_oracle(&png, "asymmetric 16-bit");
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let PngImage::Rgb16(img) = &decoded.image else {
        panic!("expected Rgb16");
    };
    assert_eq!(img.as_samples()[0], 0x01FE, "big-endian assembly");
}

/// Metadata written by libpng comes back byte-identical through the rich decode.
#[test]
fn metadata_payloads_survive_byte_identical() {
    let (width, height) = (8u32, 8u32);
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_RGB, 8, 33);
    let exif = tiny_exif();
    let icc = tiny_icc_profile();
    let xmp = r#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"/><?xpacket end="r"?>"#;
    let texts = [
        TextChunk {
            keyword: "Title",
            text: "conformance",
            kind: TextKind::Text,
        },
        TextChunk {
            keyword: "Comment",
            text: "the quick brown fox jumps over the lazy dog, compressed",
            kind: TextKind::ZTxt,
        },
        TextChunk {
            keyword: "Author",
            text: "gämut, in UTF-8",
            kind: TextKind::ITxt {
                language: "en",
                translated: "author",
                compressed: true,
            },
        },
        TextChunk {
            keyword: "XML:com.adobe.xmp",
            text: xmp,
            kind: TextKind::ITxt {
                language: "",
                translated: "",
                compressed: false,
            },
        },
    ];
    let cicp = [9u8, 16, 0, 1];
    let png = libpng_oracle::encode(
        &pixels,
        width,
        height,
        libpng_oracle::COLOR_RGB,
        8,
        &EncodeOpts {
            gamma: Some(45455),
            chromaticities: Some([31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000]),
            exif: Some(&exif),
            icc: Some(("test profile", &icc)),
            text: &texts,
            extra_chunks: &[(*b"cICP", &cicp)],
            ..EncodeOpts::default()
        },
    );
    assert_matches_oracle(&png, "metadata-laden");

    let decoded = PngDecoder::new().decode(&png).unwrap();
    assert_eq!(
        decoded.exif.as_deref(),
        Some(exif.as_slice()),
        "eXIf verbatim"
    );
    let profile = decoded.icc_profile.as_ref().expect("iCCP surfaced");
    assert_eq!(profile.name, "test profile");
    assert_eq!(profile.profile, icc, "ICC bytes after inflation");
    assert_eq!(decoded.xmp(), Some(xmp.as_bytes()), "XMP packet");
    assert_eq!(decoded.gamma, Some(45455));
    let chrm = decoded.chromaticities.expect("cHRM surfaced");
    assert_eq!(chrm.white, (31270, 32900));
    assert_eq!(chrm.red, (64000, 33000));
    assert_eq!(chrm.green, (30000, 60000));
    assert_eq!(chrm.blue, (15000, 6000));
    let cicp_parsed = decoded.cicp.expect("cICP surfaced");
    assert_eq!(
        (
            cicp_parsed.color_primaries,
            cicp_parsed.transfer_function,
            cicp_parsed.matrix_coefficients,
            cicp_parsed.full_range
        ),
        (9, 16, 0, true)
    );
    assert_eq!(decoded.texts.len(), 4, "the XMP packet is one of the texts");
    assert_eq!(decoded.texts[0].keyword, "Title");
    assert_eq!(decoded.texts[0].text, "conformance");
    assert_eq!(
        decoded.texts[1].text,
        "the quick brown fox jumps over the lazy dog, compressed"
    );
    assert_eq!(decoded.texts[2].text, "gämut, in UTF-8");
    assert_eq!(decoded.texts[2].language.as_deref(), Some("en"));
    assert_eq!(
        decoded.texts[2].translated_keyword.as_deref(),
        Some("author")
    );
}

/// sRGB rendering intent (kept apart from iCCP: libpng refuses conflicting colour-space claims).
#[test]
fn srgb_intent_survives() {
    let (width, height) = (4u32, 4u32);
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_RGB, 8, 3);
    for intent in 0..=3 {
        let png = libpng_oracle::encode(
            &pixels,
            width,
            height,
            libpng_oracle::COLOR_RGB,
            8,
            &EncodeOpts {
                srgb_intent: Some(intent),
                ..EncodeOpts::default()
            },
        );
        let decoded = PngDecoder::new().decode(&png).unwrap();
        let srgb = decoded.srgb.expect("sRGB surfaced");
        assert_eq!(srgb as u8 as i32, intent, "intent {intent}");
    }
}

/// A libpng-written interlaced fixture at the decoder's exact dimension limit decodes; one past
/// it is refused.
#[test]
fn decode_limits_hold_for_oracle_fixtures() {
    let png = fixture(24, 15, libpng_oracle::COLOR_RGB, 8, true, 8);
    assert!(
        PngDecoder::new()
            .with_max_dimensions(24, 15)
            .decode(&png)
            .is_ok(),
        "at the limit"
    );
    assert!(
        PngDecoder::new()
            .with_max_dimensions(23, 15)
            .decode(&png)
            .is_err(),
        "one past the width limit"
    );
}
