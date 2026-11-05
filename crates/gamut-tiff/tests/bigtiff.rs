//! BigTIFF (64-bit offset) conformance cross-checks (P20).
//!
//! BigTIFF only widens the container — the colour/compression/pixel layers are identical — so
//! these mirror the classic cross-checks with `.with_big_tiff(true)`:
//!   A. gamut round-trips (encode → gamut decode);
//!   B. gamut encodes BigTIFF → libtiff decodes back to the source pixels;
//!   C. libtiff encodes BigTIFF → gamut decodes (exercising 64-bit `LONG8` strip offsets).
//!
//! libtiff reads BigTIFF through the same API as classic TIFF, so its decode helpers are
//! unchanged; only its encode side opts into the "w8" mode via `encode_*_bigtiff`.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{ByteOrder, Compression, TiffDecoder, TiffEncoder, Variant, read};
use libtiff_oracle::Compression as OracleCompression;

mod common;

use common::{gray_pattern, rgb_pattern};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (16, 16), (17, 13), (64, 100)];

/// Confirms a byte stream really is BigTIFF (not silently downgraded to classic TIFF).
fn assert_is_bigtiff(tiff: &[u8]) {
    assert_eq!(
        read(tiff).expect("parse header").variant,
        Variant::Big,
        "expected a BigTIFF file (magic 43)"
    );
}

#[test]
fn bigtiff_roundtrips_in_gamut() {
    for &(w, h) in SIZES {
        let dims = Dimensions::new(w, h).unwrap();
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            // RGB.
            let rgb = rgb_pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_byte_order(order)
                .with_big_tiff(true)
                .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut tiff)
                .expect("encode");
            assert_is_bigtiff(&tiff);
            let got: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h)
            );
            assert_eq!(
                got.as_samples(),
                rgb.as_slice(),
                "BigTIFF RGB {w}x{h} {order:?}"
            );

            // Gray.
            let gray = gray_pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_byte_order(order)
                .with_big_tiff(true)
                .encode_image(ImageRef::<Gray8>::new(&gray, dims).unwrap(), &mut tiff)
                .expect("encode");
            assert_is_bigtiff(&tiff);
            let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                got.as_samples(),
                gray.as_slice(),
                "BigTIFF gray {w}x{h} {order:?}"
            );
        }
    }
}

#[test]
fn gamut_bigtiff_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let dims = Dimensions::new(w, h).unwrap();
        let rgb = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_big_tiff(true)
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut tiff)
            .expect("encode");
        assert_is_bigtiff(&tiff);
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, rgb, "BigTIFF RGB mismatch at {w}x{h}");

        let gray = gray_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_big_tiff(true)
            .encode_image(ImageRef::<Gray8>::new(&gray, dims).unwrap(), &mut tiff)
            .expect("encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 1));
        assert_eq!(dec.pixels, gray, "BigTIFF gray mismatch at {w}x{h}");
    }
}

#[test]
fn libtiff_bigtiff_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let rgb = rgb_pattern(w, h);
        let tiff = libtiff_oracle::encode_rgb8_bigtiff(&rgb, w, h, OracleCompression::None)
            .expect("libtiff encode");
        assert_is_bigtiff(&tiff); // the oracle really wrote BigTIFF ("w8")
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (w, h)
        );
        assert_eq!(
            got.as_samples(),
            rgb.as_slice(),
            "BigTIFF RGB mismatch at {w}x{h}"
        );

        let gray = gray_pattern(w, h);
        let tiff = libtiff_oracle::encode_gray8_bigtiff(&gray, w, h, OracleCompression::None)
            .expect("libtiff encode");
        assert_is_bigtiff(&tiff);
        let got: ImageBuf<Gray8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(
            got.as_samples(),
            gray.as_slice(),
            "BigTIFF gray mismatch at {w}x{h}"
        );
    }
}

#[test]
fn bigtiff_compression_variants_cross_check() {
    // The container switch is orthogonal to the strip codec: every byte-level scheme still
    // round-trips and is read back by libtiff under BigTIFF.
    let (w, h) = (40, 30);
    let rgb = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    for compression in [
        Compression::None,
        Compression::PackBits,
        Compression::Lzw,
        Compression::Deflate,
    ] {
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_big_tiff(true)
            .with_compression(compression)
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut tiff)
            .expect("encode");
        assert_is_bigtiff(&tiff);
        // gamut round-trip.
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(
            got.as_samples(),
            rgb.as_slice(),
            "BigTIFF gamut round-trip {compression:?}"
        );
        // libtiff cross-check.
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!(
            dec.pixels, rgb,
            "BigTIFF libtiff cross-check {compression:?}"
        );
    }
}

#[test]
fn bigtiff_tiled_is_decoded_by_libtiff() {
    // Multi-tile, multi-strip-region layout: more than one out-of-line offset, so the LONG8
    // offset array lands in the value pool rather than inline.
    let (w, h) = (40, 36);
    let rgb = rgb_pattern(w, h);
    let dims = Dimensions::new(w, h).unwrap();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .with_big_tiff(true)
        .with_tiling(16, 16)
        .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut tiff)
        .expect("encode");
    assert_is_bigtiff(&tiff);
    // gamut round-trip.
    let got: ImageBuf<Rgb8> = TiffDecoder::new()
        .decode_image(&tiff)
        .expect("gamut decode");
    assert_eq!(
        got.as_samples(),
        rgb.as_slice(),
        "BigTIFF tiled gamut round-trip"
    );
    // libtiff cross-check via its RGBA reader, which handles tiled images and crops padding.
    let (rw, rh, rgba) = libtiff_oracle::decode_rgba(&tiff).expect("libtiff rgba");
    assert_eq!((rw, rh), (w, h));
    let got: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert_eq!(got, rgb, "BigTIFF tiled libtiff cross-check");
}

#[test]
fn bigtiff_multipage_is_decoded_by_libtiff() {
    // Two pages chained through 8-byte next-IFD pointers, with all strip data in a shared region.
    let pages_px = [rgb_pattern(20, 12), rgb_pattern(8, 30)];
    let pages = [
        ImageRef::<Rgb8>::new(pages_px[0].as_slice(), Dimensions::new(20, 12).unwrap()).unwrap(),
        ImageRef::<Rgb8>::new(pages_px[1].as_slice(), Dimensions::new(8, 30).unwrap()).unwrap(),
    ];
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .with_big_tiff(true)
        .encode_pages_rgb8(&pages, &mut tiff)
        .expect("encode");
    assert_is_bigtiff(&tiff);

    let dec = TiffDecoder::new();
    assert_eq!(dec.page_count(&tiff).expect("pages"), 2);
    for (i, page) in pages.iter().enumerate() {
        let (px, dims) = (page.as_samples(), page.dimensions());
        // gamut round-trip.
        let got: ImageBuf<Rgb8> = dec.decode_page(&tiff, i).expect("gamut decode page");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (dims.width(), dims.height())
        );
        assert_eq!(got.as_samples(), px, "BigTIFF page {i} gamut round-trip");
        // libtiff cross-check.
        let lt = libtiff_oracle::decode_page(&tiff, i as u32).expect("libtiff decode page");
        assert_eq!((lt.width, lt.height), (dims.width(), dims.height()));
        assert_eq!(lt.pixels, px, "BigTIFF page {i} libtiff cross-check");
    }
}
