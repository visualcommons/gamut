//! Self round-trips: the gamut encoder's output — already proven byte-exact against libpng by
//! `tests/oracle.rs` — must decode back to the source through the new decoder. This pins the
//! encoder/decoder pair end to end, including the gamut-deflate ↔ miniz_oxide interop.

mod common;

use common::{noise, tiny_exif, tiny_icc_profile};
use gamut_core::{
    Bilevel, DecodeImage, Dimensions, EncodeImage, Gray8, Gray16, GrayAlpha8, GrayAlpha16,
    ImageBuf, ImageRef, Indexed8, Pixel, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_png::{
    FilterStrategy, FilterType, Level, PngDecoder, PngEncoder, PngImage, PngPalette, SrgbIntent,
};

/// Encodes `src` as `P` with `encoder`, decodes as `P`, and asserts the samples survive.
fn round_trip<P: Pixel>(encoder: &PngEncoder, src: &[P::Sample], width: u32, height: u32)
where
    PngEncoder: EncodeImage<P>,
    PngDecoder: DecodeImage<P>,
    P::Sample: std::fmt::Debug + PartialEq,
{
    let dims = Dimensions::new(width, height).unwrap();
    let mut png = Vec::new();
    encoder
        .encode_image(ImageRef::<P>::new(src, dims).unwrap(), &mut png)
        .expect("encode");
    let decoded: ImageBuf<P> = PngDecoder::new().decode_image(&png).expect("decode");
    assert_eq!(decoded.dimensions(), dims);
    assert_eq!(decoded.as_samples(), src);
}

#[test]
fn every_colour_type_and_level_round_trips() {
    let (w, h) = (23u32, 11u32);
    for level in [Level::Fast, Level::Default, Level::Best] {
        let encoder = PngEncoder::new().with_compression(level);
        let n = (w * h) as usize;
        round_trip::<Gray8>(&encoder, &noise(n, 1), w, h);
        round_trip::<GrayAlpha8>(&encoder, &noise(n * 2, 2), w, h);
        round_trip::<Rgb8>(&encoder, &noise(n * 3, 3), w, h);
        round_trip::<Rgba8>(&encoder, &noise(n * 4, 4), w, h);
        let wide: Vec<u16> = (0..n * 4).map(|i| (i * 2557) as u16).collect();
        round_trip::<Gray16>(&encoder, &wide[..n], w, h);
        round_trip::<GrayAlpha16>(&encoder, &wide[..n * 2], w, h);
        round_trip::<Rgb16>(&encoder, &wide[..n * 3], w, h);
        round_trip::<Rgba16>(&encoder, &wide, w, h);
        // Bilevel normalises on encode (non-zero → white), so decode returns 0/1.
        let bits: Vec<u8> = (0..n).map(|i| u8::from(i % 3 == 0)).collect();
        round_trip::<Bilevel>(&encoder, &bits, w, h);
    }
}

#[test]
fn effort_budget_round_trips_and_defaults_to_six_passes() {
    let (w, h) = (23u32, 11u32);
    let n = (w * h) as usize;
    // Structured-plus-noisy samples (the shape that makes the optimal parse's refinement passes
    // change the stream), filtered with `None` so they reach DEFLATE unmixed.
    let src: Vec<u8> = b"header header header "
        .iter()
        .copied()
        .cycle()
        .take(n)
        .chain((0..n as u32 * 2).map(|i| (i.wrapping_mul(48_271) >> 20) as u8))
        .collect();
    for effort in [0u8, 15] {
        let encoder = PngEncoder::new()
            .with_compression(Level::Best)
            .with_effort(effort)
            .with_filter(FilterStrategy::None);
        round_trip::<Rgb8>(&encoder, &src, w, h);
    }

    // The builder default must be exactly effort 6 — for the IDAT and for the compressed
    // ancillary streams (iCCP, zTXt), which follow the same budget — while effort 0 must
    // actually change the emitted bytes.
    let encode = |encoder: PngEncoder| {
        let mut png = Vec::new();
        encoder
            .with_compression(Level::Best)
            .with_filter(FilterStrategy::None)
            .with_icc_profile("prof", &tiny_icc_profile())
            .with_compressed_text("Comment", &"squeeze ".repeat(40))
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut png,
            )
            .expect("encode");
        png
    };
    let implicit = encode(PngEncoder::new());
    assert_eq!(implicit, encode(PngEncoder::new().with_effort(6)));
    assert_ne!(implicit, encode(PngEncoder::new().with_effort(0)));
}

#[test]
fn every_filter_strategy_round_trips() {
    let (w, h) = (32u32, 24u32);
    let src = noise((w * h * 3) as usize, 7);
    for strategy in [
        FilterStrategy::None,
        FilterStrategy::Fixed(FilterType::Sub),
        FilterStrategy::Fixed(FilterType::Up),
        FilterStrategy::Fixed(FilterType::Average),
        FilterStrategy::Fixed(FilterType::Paeth),
        FilterStrategy::MinSumAbs,
        FilterStrategy::BruteForce,
    ] {
        let encoder = PngEncoder::new().with_filter(strategy);
        round_trip::<Rgb8>(&encoder, &src, w, h);
    }
}

#[test]
fn indexed_round_trips_at_every_auto_depth() {
    for entries in [2usize, 4, 16, 17, 256] {
        let rgb: Vec<[u8; 3]> = (0..entries)
            .map(|i| [i as u8, (i * 11) as u8, (i * 29) as u8])
            .collect();
        let alpha: Vec<u8> = (0..entries.min(5)).map(|i| (i * 60) as u8).collect();
        let palette = PngPalette::with_transparency(&rgb, &alpha).unwrap();
        let (w, h) = (21u32, 9u32);
        let indices: Vec<u8> = (0..(w * h) as usize).map(|i| (i % entries) as u8).collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_indexed8(
                ImageRef::<Indexed8>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap(),
                &palette,
                &mut png,
            )
            .expect("encode");
        let decoded: ImageBuf<Indexed8> = PngDecoder::new().decode_image(&png).expect("decode");
        assert_eq!(decoded.as_samples(), indices, "{entries} entries");
        // And through the rich path, the palette itself survives.
        let rich = PngDecoder::new().decode(&png).unwrap();
        let carried = rich.palette.expect("palette carried");
        assert_eq!(carried.len(), entries);
        for (i, &[r, g, b]) in rgb.iter().enumerate() {
            assert_eq!(carried.rgb(i as u8), Some([r, g, b]));
            let expected_alpha = alpha.get(i).copied().unwrap_or(255);
            assert_eq!(carried.alpha(i as u8), Some(expected_alpha));
        }
    }
}

#[test]
fn auto_reduced_output_decodes_back_to_the_rgba_source() {
    let (w, h) = (32u32, 32u32);
    let n = (w * h) as usize;
    // The same three reduction shapes the encoder oracle suite uses: grey, palette+tRNS, RGB.
    let gray: Vec<u8> = (0..n)
        .flat_map(|i| {
            let v = (i * 5) as u8;
            [v, v, v, 255]
        })
        .collect();
    let palette: Vec<u8> = (0..n)
        .flat_map(|i| match i % 3 {
            0 => [200, 0, 0, 255],
            1 => [0, 200, 0, 128],
            _ => [0, 0, 200, 255],
        })
        .collect();
    let opaque: Vec<u8> = (0..n)
        .flat_map(|i| {
            [
                (i % 251) as u8,
                (i % 241) as u8,
                (i % headroom(i)) as u8,
                255,
            ]
        })
        .collect();
    let dims = Dimensions::new(w, h).unwrap();
    for (name, src) in [("gray", &gray), ("palette", &palette), ("opaque", &opaque)] {
        let mut png = Vec::new();
        PngEncoder::new()
            .with_auto_reduce(true)
            .encode_image(ImageRef::<Rgba8>::new(src, dims).unwrap(), &mut png)
            .expect("encode");
        // Whatever colour type the reducer picked, RGBA widening restores the source exactly.
        let decoded: ImageBuf<Rgba8> = PngDecoder::new().decode_image(&png).expect("decode");
        assert_eq!(&decoded.as_samples().to_vec(), src, "{name}");
    }
}

/// A varying modulus so the "opaque" image has too many distinct colours to palette-reduce.
fn headroom(i: usize) -> usize {
    193 + (i % 17)
}

#[test]
fn auto_reduce_round_trips_grey_and_sixteen_bit_layouts() {
    // Every extended reduction shape decodes back **as the source pixel type**: sub-byte grey and
    // grey palettes rewiden through the Gray8/GrayAlpha8 impls, and demoted 16-bit files through
    // the ×257 widening in the 16-bit impls. Width 19 exercises sub-byte row padding.
    let (w, h) = (19u32, 7u32);
    let n = (w * h) as usize;
    let encoder = PngEncoder::new().with_auto_reduce(true);

    // Gray8 -> 1-bit grey; off-grid low-cardinality grey -> grey palette.
    let bw: Vec<u8> = (0..n).map(|i| if i % 3 == 0 { 255 } else { 0 }).collect();
    round_trip::<Gray8>(&encoder, &bw, w, h);
    let off_grid: Vec<u8> = (0..n).map(|i| [5u8, 9, 200][i % 3]).collect();
    round_trip::<Gray8>(&encoder, &off_grid, w, h);

    // GrayAlpha8 -> alpha drop; two (grey, alpha) combinations -> grey palette with tRNS.
    let ga_opaque: Vec<u8> = (0..n).flat_map(|i| [(i % 89) as u8, 255]).collect();
    round_trip::<GrayAlpha8>(&encoder, &ga_opaque, w, h);
    let ga_keyed: Vec<u8> = (0..n)
        .flat_map(|i| if i % 2 == 0 { [0u8, 0] } else { [255, 255] })
        .collect();
    round_trip::<GrayAlpha8>(&encoder, &ga_keyed, w, h);

    // Demotable 16-bit inputs: grey recursion, plain demotion, and a Gray16 source.
    let gray16: Vec<u16> = (0..n).map(|i| ((i % 60) as u16) * 257).collect();
    round_trip::<Gray16>(&encoder, &gray16, w, h);
    let rgba16_gray: Vec<u16> = (0..n)
        .flat_map(|i| {
            let v = ((i % 60) as u16) * 257;
            [v, v, v, u16::MAX]
        })
        .collect();
    round_trip::<Rgba16>(&encoder, &rgba16_gray, w, h);
    let rgba16_demoted: Vec<u16> = (0..n)
        .flat_map(|i| {
            [
                ((i % 251) as u16) * 257,
                ((i % 241) as u16) * 257,
                ((i % 239) as u16) * 257,
                ((i % 233) as u16) * 257,
            ]
        })
        .collect();
    round_trip::<Rgba16>(&encoder, &rgba16_demoted, w, h);

    // Non-demotable 16-bit-native reductions: grey and alpha-drop at full depth.
    let rgb16_gray: Vec<u16> = (0..n)
        .flat_map(|i| {
            let v = (i * 501 + 1) as u16;
            [v, v, v]
        })
        .collect();
    round_trip::<Rgb16>(&encoder, &rgb16_gray, w, h);
    let ga16_opaque: Vec<u16> = (0..n)
        .flat_map(|i| [(i * 501 + 1) as u16, u16::MAX])
        .collect();
    round_trip::<GrayAlpha16>(&encoder, &ga16_opaque, w, h);
    let rgba16_rgb: Vec<u16> = (0..n)
        .flat_map(|i| [(i * 501 + 1) as u16, (i * 703 + 2) as u16, 3, u16::MAX])
        .collect();
    round_trip::<Rgba16>(&encoder, &rgba16_rgb, w, h);
}

#[test]
fn ancillary_pile_survives_decode() {
    let (w, h) = (16u32, 16u32);
    let src = noise((w * h * 3) as usize, 9);
    let exif = tiny_exif();
    let icc = tiny_icc_profile();
    let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"/>"#;
    let mut png = Vec::new();
    PngEncoder::new()
        .with_gamma(1.0 / 2.2)
        .with_srgb(SrgbIntent::RelativeColorimetric)
        .with_chromaticities((0.3127, 0.3290), (0.64, 0.33), (0.30, 0.60), (0.15, 0.06))
        .with_significant_bits(&[8, 8, 8])
        .with_background_rgb(0, 0, 0)
        .with_physical_dimensions(2835, 2835, gamut_png::PhysicalUnit::Meter)
        .with_time(2026, 7, 17, 1, 2, 3)
        .with_text("Title", "gamut")
        .with_compressed_text("Comment", &"squeeze ".repeat(40))
        .with_international_text("Author", "gämut")
        .with_exif(&exif)
        .with_icc_profile("prof", &icc)
        .with_xmp(xmp)
        .encode_image(
            ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut png,
        )
        .expect("encode");
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let PngImage::Rgb8(img) = &decoded.image else {
        panic!("expected Rgb8");
    };
    assert_eq!(img.as_samples(), src, "pixels survive the ancillary pile");
    assert_eq!(decoded.gamma, Some(45455));
    assert_eq!(decoded.srgb, Some(SrgbIntent::RelativeColorimetric));
    assert!(decoded.chromaticities.is_some());
    assert_eq!(decoded.exif.as_deref(), Some(exif.as_slice()));
    assert_eq!(decoded.icc_profile.unwrap().profile, icc);
    assert_eq!(decoded.xmp.as_deref(), Some(xmp.as_bytes()));
    assert_eq!(decoded.texts.len(), 3);
}
