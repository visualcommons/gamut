//! Differential conformance cross-checks against a vendored **libpng**.
//!
//! gamut ships no PNG decoder, so correctness is proven by decoding the encoder's output with libpng
//! and asserting the pixels (and IHDR fields) match the source exactly.

use gamut_core::{
    Bilevel, Dimensions, EncodeImage, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageRef, Indexed8,
    Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_deflate::Level;
use gamut_png::{FilterStrategy, FilterType, PhysicalUnit, PngEncoder, PngPalette, SrgbIntent};

const SIZES: &[(u32, u32)] = &[
    (1, 1),
    (2, 2),
    (3, 7),
    (16, 16),
    (17, 13),
    (64, 100),
    (100, 70),
];

/// Whether a chunk of the given 4-byte type appears in the PNG stream.
fn contains_chunk(png: &[u8], ty: &[u8; 4]) -> bool {
    let mut i = 8; // skip the signature
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        if &png[i + 4..i + 8] == ty {
            return true;
        }
        i += 12 + len;
    }
    false
}

/// A deterministic RGB pattern with enough structure to exercise filtering and matching.
fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(31).wrapping_add(y)) as u8);
            v.push((y.wrapping_mul(17) ^ x) as u8);
            v.push((x.wrapping_add(y).wrapping_mul(5)) as u8);
        }
    }
    v
}

#[test]
fn gamut_rgb8_is_decoded_by_libpng() {
    for &(w, h) in SIZES {
        for level in [Level::Fast, Level::Default, Level::Best] {
            let src = rgb_pattern(w, h);
            let dims = Dimensions::new(w, h).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .with_compression(level)
                .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
                .expect("encode");

            let dec = libpng_oracle::decode(&png);
            assert_eq!((dec.width, dec.height), (w, h), "dims {w}x{h} {level:?}");
            assert_eq!(
                dec.color_type,
                libpng_oracle::COLOR_RGB,
                "{w}x{h} {level:?}"
            );
            assert_eq!(dec.bit_depth, 8, "{w}x{h} {level:?}");
            assert_eq!(dec.rowbytes, w as usize * 3, "{w}x{h} {level:?}");
            assert_eq!(dec.pixels, src, "pixels {w}x{h} {level:?}");
        }
    }
}

#[test]
fn every_filter_strategy_round_trips() {
    let (w, h) = (32, 24);
    let src = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    let strategies = [
        FilterStrategy::None,
        FilterStrategy::Fixed(FilterType::Sub),
        FilterStrategy::Fixed(FilterType::Up),
        FilterStrategy::Fixed(FilterType::Average),
        FilterStrategy::Fixed(FilterType::Paeth),
        FilterStrategy::MinSumAbs,
    ];
    for strategy in strategies {
        let mut png = Vec::new();
        PngEncoder::new()
            .with_filter(strategy)
            .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.pixels, src, "{strategy:?}");
    }
}

#[test]
fn filtering_shrinks_a_gradient() {
    // A smooth gradient compresses much better filtered than unfiltered.
    let (w, h) = (64u32, 64u32);
    let mut src = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            src.push((x * 4) as u8);
            src.push((y * 4) as u8);
            src.push(((x + y) * 2) as u8);
        }
    }
    let dims = Dimensions::new(w, h).unwrap();
    let encode = |strategy| {
        let mut png = Vec::new();
        PngEncoder::new()
            .with_filter(strategy)
            .with_compression(Level::Best)
            .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
            .expect("encode");
        png.len()
    };
    let unfiltered = encode(FilterStrategy::None);
    let filtered = encode(FilterStrategy::MinSumAbs);
    assert!(
        filtered < unfiltered,
        "filtered {filtered} should beat unfiltered {unfiltered}"
    );
}

/// Encodes an 8-bit image of pixel type `$ty` and asserts libpng decodes the exact source bytes
/// with the expected colour type.
macro_rules! check_8bit {
    ($ty:ty, $channels:expr, $color:expr) => {{
        let (w, h) = (20u32, 15u32);
        let n = (w * h) as usize * $channels;
        let src: Vec<u8> = (0..n)
            .map(|i| (i.wrapping_mul(37) ^ (i >> 2)) as u8)
            .collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_compression(Level::Best)
            .encode_image(
                ImageRef::<$ty>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut png,
            )
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.bit_depth, 8, "{}", stringify!($ty));
        assert_eq!(dec.color_type, $color, "{}", stringify!($ty));
        assert_eq!(dec.pixels, src, "{}", stringify!($ty));
    }};
}

/// As [`check_8bit`] but for 16-bit samples, comparing against the big-endian serialisation.
macro_rules! check_16bit {
    ($ty:ty, $channels:expr, $color:expr) => {{
        let (w, h) = (18u32, 13u32);
        let n = (w * h) as usize * $channels;
        let src: Vec<u16> = (0..n).map(|i| (i.wrapping_mul(1009)) as u16).collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_compression(Level::Best)
            .encode_image(
                ImageRef::<$ty>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut png,
            )
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.bit_depth, 16, "{}", stringify!($ty));
        assert_eq!(dec.color_type, $color, "{}", stringify!($ty));
        let expected: Vec<u8> = src.iter().flat_map(|s| s.to_be_bytes()).collect();
        assert_eq!(dec.pixels, expected, "{}", stringify!($ty));
    }};
}

#[test]
fn eight_bit_colour_types_round_trip() {
    check_8bit!(Gray8, 1, libpng_oracle::COLOR_GRAY);
    check_8bit!(GrayAlpha8, 2, libpng_oracle::COLOR_GRAY_ALPHA);
    check_8bit!(Rgb8, 3, libpng_oracle::COLOR_RGB);
    check_8bit!(Rgba8, 4, libpng_oracle::COLOR_RGBA);
}

#[test]
fn sixteen_bit_colour_types_round_trip() {
    check_16bit!(Gray16, 1, libpng_oracle::COLOR_GRAY);
    check_16bit!(GrayAlpha16, 2, libpng_oracle::COLOR_GRAY_ALPHA);
    check_16bit!(Rgb16, 3, libpng_oracle::COLOR_RGB);
    check_16bit!(Rgba16, 4, libpng_oracle::COLOR_RGBA);
}

#[test]
fn indexed8_with_palette_and_transparency_round_trips() {
    let (w, h) = (24u32, 18u32);
    let rgb: Vec<[u8; 3]> = vec![
        [10, 20, 30],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [128, 128, 128],
    ];
    let alpha = vec![0u8, 255, 128]; // entries 0/1/2 have alpha; 3/4 are opaque
    let palette = PngPalette::with_transparency(&rgb, &alpha).unwrap();
    let indices: Vec<u8> = (0..(w * h) as usize)
        .map(|i| (i % rgb.len()) as u8)
        .collect();

    let mut png = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_indexed8(
            ImageRef::<Indexed8>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap(),
            &palette,
            &mut png,
        )
        .expect("encode");

    // Raw decode: it is a palette image and the indices survive exactly. Five entries fit in a
    // 4-bit index, which the encoder selects automatically.
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.color_type, libpng_oracle::COLOR_PALETTE);
    assert_eq!(dec.bit_depth, 4);
    assert_eq!(dec.pixels, indices);

    // Expanded decode: palette colours and tRNS resolve to the intended RGBA.
    let (dw, dh, rgba) = libpng_oracle::decode_rgba8(&png);
    assert_eq!((dw, dh), (w, h));
    let expected: Vec<u8> = indices
        .iter()
        .flat_map(|&idx| {
            let [r, g, b] = rgb[idx as usize];
            let a = alpha.get(idx as usize).copied().unwrap_or(255);
            [r, g, b, a]
        })
        .collect();
    assert_eq!(rgba, expected);
}

#[test]
fn indexed8_rejects_out_of_range_index() {
    let palette = PngPalette::new(&[[0, 0, 0], [255, 255, 255]]).unwrap();
    let indices = vec![0u8, 1, 2]; // 2 is out of range for a 2-entry palette
    let mut png = Vec::new();
    let result = PngEncoder::new().encode_indexed8(
        ImageRef::<Indexed8>::new(&indices, Dimensions::new(3, 1).unwrap()).unwrap(),
        &palette,
        &mut png,
    );
    assert!(result.is_err());
}

#[test]
fn indexed_uses_minimal_bit_depth() {
    // Palette size determines the smallest index bit depth; libpng must report it and recover the
    // indices (png_set_packing unpacks sub-byte indices to one byte each).
    for (entries, depth) in [(2usize, 1u8), (4, 2), (16, 4), (17, 8)] {
        let rgb: Vec<[u8; 3]> = (0..entries).map(|i| [i as u8, 0, 0]).collect();
        let palette = PngPalette::new(&rgb).unwrap();
        let (w, h) = (20u32, 8u32);
        let indices: Vec<u8> = (0..(w * h) as usize).map(|i| (i % entries) as u8).collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_indexed8(
                ImageRef::<Indexed8>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap(),
                &palette,
                &mut png,
            )
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.color_type, libpng_oracle::COLOR_PALETTE, "{entries}");
        assert_eq!(dec.bit_depth, depth, "{entries} entries");
        assert_eq!(dec.pixels, indices, "{entries} entries");
    }
}

#[test]
fn bilevel_round_trips_as_1bit_gray() {
    // A non-byte-aligned width exercises sub-byte row padding.
    let (w, h) = (19u32, 7u32);
    let src: Vec<u8> = (0..(w * h) as usize)
        .map(|i| u8::from(i % 3 == 0) * 200)
        .collect();
    let mut png = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(
            ImageRef::<Bilevel>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut png,
        )
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.color_type, libpng_oracle::COLOR_GRAY);
    assert_eq!(dec.bit_depth, 1);
    let expected: Vec<u8> = src.iter().map(|&v| u8::from(v != 0)).collect();
    assert_eq!(dec.pixels, expected);
}

#[test]
fn ancillary_chunks_are_accepted_by_libpng() {
    // Pile on every standard ancillary chunk. libpng validates them on read (and decompresses
    // zTXt/iTXt internally), so if any chunk were malformed the oracle would abort. The pixels must
    // also survive unchanged.
    let (w, h) = (16u32, 16u32);
    let src = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    let comment = "the quick brown fox ".repeat(20);
    let mut png = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_gamma(1.0 / 2.2)
        .with_srgb(SrgbIntent::Perceptual)
        .with_chromaticities((0.3127, 0.3290), (0.64, 0.33), (0.30, 0.60), (0.15, 0.06))
        .with_significant_bits(&[8, 8, 8])
        .with_background_rgb(0, 0, 0)
        .with_physical_dimensions(2835, 2835, PhysicalUnit::Meter)
        .with_time(2026, 6, 13, 1, 2, 3)
        .with_text("Title", "gamut")
        .with_compressed_text("Comment", &comment)
        .with_international_text("Author", "gämut")
        .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.pixels, src);
}

#[test]
fn metadata_chunks_embed_and_image_survives() {
    let (w, h) = (12u32, 12u32);
    let src = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();

    // Minimal-but-plausible EXIF (TIFF header + empty IFD) and ICC profile (132-byte header).
    let exif = [
        0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut icc = vec![0u8; 132];
    icc[0..4].copy_from_slice(&132u32.to_be_bytes()); // profile size
    icc[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes()); // version 2.1
    icc[12..16].copy_from_slice(b"mntr");
    icc[16..20].copy_from_slice(b"RGB ");
    icc[20..24].copy_from_slice(b"XYZ ");
    icc[36..40].copy_from_slice(b"acsp"); // ICC signature
    let xmp = r#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"></x:xmpmeta><?xpacket end="r"?>"#;

    let mut png = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_exif(&exif)
        .with_icc_profile("test profile", &icc)
        .with_xmp(xmp)
        .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
        .expect("encode");

    assert!(contains_chunk(&png, b"eXIf"), "eXIf present");
    assert!(contains_chunk(&png, b"iCCP"), "iCCP present");
    assert!(contains_chunk(&png, b"iTXt"), "iTXt (XMP) present");

    // libpng parses every chunk (decompressing iCCP); the image must survive unchanged.
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.pixels, src);
}

/// The three inputs auto-reduce is meant to recognise, each with the colour type it should pick.
///
/// Shared by the claims below so none of them carries a fixture whose construction is another
/// claim's subject.
struct AutoReduceCase {
    name: &'static str,
    rgba: Vec<u8>,
    expected_type: u8,
}

fn auto_reduce_cases() -> (Dimensions, [AutoReduceCase; 3]) {
    let (w, h) = (32u32, 32u32);
    let n = (w * h) as usize;

    // Opaque greyscale stored as RGBA -> should reduce to greyscale.
    let gray: Vec<u8> = (0..n)
        .flat_map(|i| {
            let v = (i * 5) as u8;
            [v, v, v, 255]
        })
        .collect();
    // Three colours, one translucent -> palette + tRNS.
    let palette: Vec<u8> = (0..n)
        .flat_map(|i| match i % 3 {
            0 => [200, 0, 0, 255],
            1 => [0, 200, 0, 128],
            _ => [0, 0, 200, 255],
        })
        .collect();
    // Opaque, many distinct, non-grey -> drop the alpha channel (RGB).
    let mut opaque = Vec::with_capacity(n * 4);
    for y in 0..h {
        for x in 0..w {
            opaque.extend_from_slice(&[x as u8, y as u8, (x * y) as u8, 255]);
        }
    }

    let dims = Dimensions::new(w, h).unwrap();
    (
        dims,
        [
            AutoReduceCase {
                name: "gray",
                rgba: gray,
                expected_type: libpng_oracle::COLOR_GRAY,
            },
            AutoReduceCase {
                // Three colours repeating with period 3: DEFLATE squeezes the RGBA stream to
                // less than the palette encoding's PLTE + tRNS + framing costs on its own, so
                // `write_reduced_or_native` keeps the unreduced form. That is the smaller file,
                // which is the contract; `a_palette_is_chosen_when_it_actually_wins` covers the
                // other side of that race, and `reduce`'s own unit tests pin the analysis.
                name: "palette",
                rgba: palette,
                expected_type: libpng_oracle::COLOR_RGBA,
            },
            AutoReduceCase {
                name: "opaque",
                rgba: opaque,
                expected_type: libpng_oracle::COLOR_RGB,
            },
        ],
    )
}

/// Encodes `src` as RGBA with auto-reduce on.
fn encode_auto_reduced(src: &[u8], dims: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_auto_reduce(true)
        .encode_image(ImageRef::<Rgba8>::new(src, dims).unwrap(), &mut out)
        .expect("encode");
    out
}

#[test]
fn auto_reduce_picks_the_colour_type_the_pixels_allow() {
    let (dims, cases) = auto_reduce_cases();

    for case in &cases {
        let reduced = encode_auto_reduced(&case.rgba, dims);

        assert_eq!(
            libpng_oracle::decode(&reduced).color_type,
            case.expected_type,
            "{}: reduced to the expected colour type",
            case.name
        );
    }
}

/// The palette side of `write_reduced_or_native`'s race.
///
/// A palette costs a flat `PLTE` (+ `tRNS`) that DEFLATE cannot compress, so whether it wins is
/// size-dependent: the fixed cost has to be amortised over enough pixels. At 32x32 it is not, and
/// the cases above keep the unreduced form; at 192x192 with the same colour count it is, and the
/// encoder must take the palette. Without this test the palette encoding path would only ever be
/// exercised where it loses.
#[test]
fn a_palette_is_chosen_when_it_actually_wins() {
    let (w, h) = (192u32, 192u32);
    let dims = Dimensions::new(w, h).unwrap();
    // 64 distinct colours in 8x8 blocks: too many for RGBA to compress away, few enough to index.
    let mut src = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let idx = ((x / 8 + y / 8 * 8) % 64) as u8;
            src.extend_from_slice(&[
                idx.wrapping_mul(4),
                idx.wrapping_mul(9),
                255 - idx.wrapping_mul(3),
                255,
            ]);
        }
    }

    let reduced = encode_auto_reduced(&src, dims);
    assert_eq!(
        libpng_oracle::decode(&reduced).color_type,
        libpng_oracle::COLOR_PALETTE,
        "the palette wins once its fixed cost is amortised"
    );

    let mut plain = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(ImageRef::<Rgba8>::new(&src, dims).unwrap(), &mut plain)
        .expect("encode");
    assert!(
        reduced.len() < plain.len(),
        "and it is smaller: {} vs {}",
        reduced.len(),
        plain.len()
    );

    let (_, _, rgba) = libpng_oracle::decode_rgba8(&reduced);
    assert_eq!(rgba, src, "the palette resolves losslessly");
}

#[test]
fn auto_reduce_is_lossless() {
    // The claim that makes the reduction safe to enable at all: whatever colour type it chose,
    // resolving the result back to RGBA must reproduce the source exactly. A reduction that
    // picked the right type and quantised while doing it would satisfy the test above.
    let (dims, cases) = auto_reduce_cases();

    for case in &cases {
        let reduced = encode_auto_reduced(&case.rgba, dims);

        let (_, _, rgba) = libpng_oracle::decode_rgba8(&reduced);
        assert_eq!(rgba, case.rgba, "{}: reduction is lossless", case.name);
    }
}

#[test]
fn auto_reduce_makes_a_greyscale_image_strictly_smaller() {
    // Greyscale reduction (3 varying channels -> 1) is the robust size win, and the reason
    // auto-reduce exists. Palette and alpha-drop can merely tie an already-tiny RGBA stream once
    // DEFLATE has exploited the redundancy, so only the greyscale case is asserted strictly.
    let (dims, cases) = auto_reduce_cases();
    let gray = &cases[0].rgba;

    let reduced = encode_auto_reduced(gray, dims);
    let mut full = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(ImageRef::<Rgba8>::new(gray, dims).unwrap(), &mut full)
        .expect("encode");

    assert!(
        reduced.len() < full.len(),
        "gray: reduced {} should beat full {}",
        reduced.len(),
        full.len()
    );
}

#[test]
fn extended_auto_reduce_covers_grey_and_sixteen_bit_inputs() {
    // A non-byte-aligned width exercises sub-byte row padding in the reduced outputs.
    let (w, h) = (19u32, 13u32);
    let n = (w * h) as usize;
    let dims = Dimensions::new(w, h).unwrap();
    let encoder = || {
        PngEncoder::new()
            .with_compression(Level::Best)
            .with_auto_reduce(true)
    };

    // Gray8 on the §13.12 grids -> sub-byte grey; libpng reports the packed depth and the codes.
    for (scale, depth) in [(255u8, 1u8), (85, 2), (17, 4)] {
        let levels = 256 / u16::from(scale) + 1; // 2, 4, or 16 representable values
        let src: Vec<u8> = (0..n)
            .map(|i| (i % levels as usize) as u8 * scale)
            .collect();
        let mut png = Vec::new();
        encoder()
            .encode_image(ImageRef::<Gray8>::new(&src, dims).unwrap(), &mut png)
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.color_type, libpng_oracle::COLOR_GRAY, "scale {scale}");
        assert_eq!(dec.bit_depth, depth, "scale {scale}");
        let codes: Vec<u8> = src.iter().map(|&v| v / scale).collect();
        assert_eq!(dec.pixels, codes, "scale {scale}");
        // No strict size assertion: the reducer compares *raw* byte estimates, and on a fixture
        // this tiny and regular DEFLATE can squeeze the 8-bit stream to within a few bytes of the
        // packed one. The depth/pixel checks above pin the contract that matters.
    }

    // Low-cardinality grey off the scale grid. `reduce::analyze8` offers a 2-bit grey palette,
    // but on a fixture this small and this regular the plain 8-bit grey stream compresses to less
    // than the palette's PLTE and framing, so `write_reduced_or_native` keeps grey. Asserted
    // exactly: no input reaches this line and comes back paletted, so admitting that as an
    // alternative would be a branch nothing can take. The size at which a palette does win, and
    // is packed below 8 bits, is covered by its own test at the end of this file.
    let off_grid: Vec<u8> = (0..n).map(|i| [5u8, 9, 200][i % 3]).collect();
    let mut png = Vec::new();
    encoder()
        .encode_image(ImageRef::<Gray8>::new(&off_grid, dims).unwrap(), &mut png)
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(
        dec.color_type,
        libpng_oracle::COLOR_GRAY,
        "off-grid grey stays grey at this size"
    );
    let (_, _, rgba) = libpng_oracle::decode_rgba8(&png);
    let expected: Vec<u8> = off_grid.iter().flat_map(|&v| [v, v, v, 255]).collect();
    assert_eq!(rgba, expected, "off-grid grey resolves losslessly");

    // GrayAlpha8 with an all-opaque alpha channel -> plain 8-bit grey.
    let ga: Vec<u8> = (0..n).flat_map(|i| [(i % 89) as u8, 255]).collect();
    let mut png = Vec::new();
    encoder()
        .encode_image(ImageRef::<GrayAlpha8>::new(&ga, dims).unwrap(), &mut png)
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.color_type, libpng_oracle::COLOR_GRAY);
    assert_eq!(dec.bit_depth, 8);
    let grays: Vec<u8> = ga.as_chunks::<2>().0.iter().map(|px| px[0]).collect();
    assert_eq!(dec.pixels, grays);

    // Rgba16 with every sample k*257, grey and opaque -> demoted all the way to 8-bit grey.
    let rgba16: Vec<u16> = (0..n)
        .flat_map(|i| {
            let v = ((i % 60) as u16) * 257;
            [v, v, v, u16::MAX]
        })
        .collect();
    let mut png = Vec::new();
    encoder()
        .encode_image(ImageRef::<Rgba16>::new(&rgba16, dims).unwrap(), &mut png)
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.color_type, libpng_oracle::COLOR_GRAY);
    assert_eq!(dec.bit_depth, 8);
    assert_eq!(
        dec.pixels,
        (0..n).map(|i| (i % 60) as u8).collect::<Vec<_>>()
    );

    // The demotion must beat the unreduced 16-bit encoding.
    let mut full = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(ImageRef::<Rgba16>::new(&rgba16, dims).unwrap(), &mut full)
        .expect("encode");
    assert!(
        png.len() < full.len(),
        "demoted {} < full {}",
        png.len(),
        full.len()
    );

    // Non-demotable opaque RGBA16 -> the alpha channel is dropped at 16 bits.
    let deep: Vec<u16> = (0..n)
        .flat_map(|i| [(i * 501 + 1) as u16, (i * 703 + 2) as u16, 3, u16::MAX])
        .collect();
    let mut png = Vec::new();
    encoder()
        .encode_image(ImageRef::<Rgba16>::new(&deep, dims).unwrap(), &mut png)
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.color_type, libpng_oracle::COLOR_RGB);
    assert_eq!(dec.bit_depth, 16);
    let expected: Vec<u8> = deep
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [px[0], px[1], px[2]])
        .flat_map(u16::to_be_bytes)
        .collect();
    assert_eq!(dec.pixels, expected);
}

#[test]
fn brute_force_filtering_round_trips_and_is_competitive() {
    let (w, h) = (48u32, 48u32);
    let src = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    let encode = |filter| {
        let mut png = Vec::new();
        PngEncoder::new()
            .with_compression(Level::Best)
            .with_auto_reduce(false)
            .with_filter(filter)
            .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
            .expect("encode");
        png
    };
    let brute = encode(FilterStrategy::BruteForce);
    assert_eq!(
        libpng_oracle::decode(&brute).pixels,
        src,
        "brute-force round-trip"
    );
    // Brute force tries MinSumAbs among its candidates, so it never loses to it.
    assert!(brute.len() <= encode(FilterStrategy::MinSumAbs).len());
}

#[test]
fn solid_image_round_trips() {
    // A flat colour is the highly-compressible extreme; libpng must still recover it exactly.
    let (w, h) = (40, 30);
    let src = vec![0x7Fu8; (w * h * 3) as usize];
    let mut png = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(
            ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut png,
        )
        .expect("encode");
    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.pixels, src);
}

#[test]
fn every_filter_strategy_survives_the_libpng_round_trip() {
    // The end-to-end pin whose absence hid a silent-corruption defect: `MinEntropy` was scored but
    // never encoded with, so nothing noticed that a row whose candidates all tied emitted its
    // predecessor's residuals. Sweeping the whole enum means a new strategy cannot land unproven.
    //
    // Deliberately narrow: 3x7 is the smallest corpus size whose rows are short enough for an
    // all-distinct-bytes tie, which is exactly the case that used to break.
    let (w, h) = (3, 7);
    let src = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    for strategy in [
        FilterStrategy::None,
        FilterStrategy::Fixed(FilterType::None),
        FilterStrategy::Fixed(FilterType::Sub),
        FilterStrategy::Fixed(FilterType::Up),
        FilterStrategy::Fixed(FilterType::Average),
        FilterStrategy::Fixed(FilterType::Paeth),
        FilterStrategy::MinSumAbs,
        FilterStrategy::MinEntropy,
        FilterStrategy::MinBigrams,
        FilterStrategy::BruteForce,
    ] {
        let mut png = Vec::new();
        PngEncoder::new()
            .with_filter(strategy)
            .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut png)
            .expect("encode");
        let dec = libpng_oracle::decode(&png);
        assert_eq!(dec.pixels, src, "{strategy:?} did not round-trip");
    }
}

/// Sub-byte indexed auto-reduce: the palette wins *and* its index depth drops below 8.
///
/// `a_palette_is_chosen_when_it_actually_wins` needs 64 colours to make the palette win, which is
/// depth 8 -- so the encoder's `depth < 8` path into `pack::pack_scanlines`, and
/// `reduce::index_bit_depth`'s `3..=4 => 2` arm, were only reached by inputs whose palette the
/// race then declined.
///
/// Four colours, and **pseudo-random** rather than blocked. Blocked, the RGBA stream compresses
/// away and `write_reduced_or_native` correctly keeps it -- which is exactly why the 64-colour
/// fixture needed 64 colours. Scattered, the four-symbol stream is near its entropy either way,
/// so the 2-bit packing is the whole difference. Measured at 192x192, `Level::Best`: 9500 bytes
/// indexed (36 864 pixels at two bits is 9216 of payload) against 19 135 as RGBA, about 50%.
#[test]
fn a_small_palette_is_packed_to_a_sub_byte_index_depth() {
    let (w, h) = (192u32, 192u32);
    let dims = Dimensions::new(w, h).unwrap();
    const PALETTE: [[u8; 4]; 4] = [
        [220, 30, 40, 255],
        [30, 200, 60, 255],
        [40, 60, 210, 255],
        [200, 190, 20, 255],
    ];
    let mut src = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            // A finalizer-quality avalanche over the pixel index. A cheaper mix (one multiply
            // and a shift) is periodic in x, and DEFLATE finds the period: the same fixture came
            // out at 272 bytes, which would have proved nothing about packing.
            let mut hash = y * w + x;
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x7feb_352d);
            hash ^= hash >> 15;
            hash = hash.wrapping_mul(0x846c_a68b);
            hash ^= hash >> 16;
            src.extend_from_slice(&PALETTE[(hash & 3) as usize]);
        }
    }

    let reduced = encode_auto_reduced(&src, dims);
    let dec = libpng_oracle::decode(&reduced);
    assert_eq!(
        dec.color_type,
        libpng_oracle::COLOR_PALETTE,
        "four colours over 36 864 pixels is a palette"
    );
    assert_eq!(dec.bit_depth, 2, "and four entries need only two bits");
    assert_eq!(
        read_chunk(&reduced, b"PLTE").expect("PLTE present").len(),
        12,
        "four RGB triples"
    );

    let mut plain = Vec::new();
    PngEncoder::new()
        .with_compression(Level::Best)
        .encode_image(ImageRef::<Rgba8>::new(&src, dims).unwrap(), &mut plain)
        .expect("encode");
    assert!(
        reduced.len() < plain.len(),
        "packed indices beat RGBA: {} vs {}",
        reduced.len(),
        plain.len()
    );

    let (_, _, rgba) = libpng_oracle::decode_rgba8(&reduced);
    assert_eq!(rgba, src, "the packed palette resolves losslessly");
}

/// The payload of the first chunk of this type, if present.
fn read_chunk(png: &[u8], want: &[u8; 4]) -> Option<Vec<u8>> {
    let mut at = 8usize;
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
        if &png[at + 4..at + 8] == want {
            return Some(png[at + 8..at + 8 + len].to_vec());
        }
        at += 12 + len;
    }
    None
}
