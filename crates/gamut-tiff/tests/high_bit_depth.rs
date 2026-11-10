//! 16-bit samples (§19): native decode over strips and tiles in both byte orders, the
//! widening/narrowing presentation policy, and bidirectional libtiff parity.

use gamut_core::convert::ConvertPolicy;
use gamut_core::{
    Cmyk8, DecodeImage, Dimensions, EncodeImage, Gray8, Gray16, ImageBuf, ImageRef, Rgb8, Rgb16,
    Rgba16,
};
use gamut_tiff::{
    ByteOrder, Compression, Ifd, PhotometricInterpretation, Predictor, SampleFormat, TiffDecoder,
    TiffEncoder, Value, Variant, read, tags, write_image,
};
use libtiff_oracle::Compression as OC;

/// Sizes include 1x1, a size smaller than one tile, and 17x13 — deliberately not a multiple of 16,
/// so every tiled case exercises the cropped right and bottom edge tiles.
const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (17, 13), (40, 24)];

/// A 16-bit pattern whose low byte varies independently of its high byte.
///
/// That independence is the point: a value like `i * 257` has identical bytes, so a byte-swap bug
/// would round-trip it unchanged and every endianness test built on it would pass vacuously.
fn pattern16(count: usize, salt: u32) -> Vec<u16> {
    (0..count)
        .map(|i| {
            let i = i as u32;
            ((i.wrapping_mul(2711).wrapping_add(salt.wrapping_mul(7919))) % 65536) as u16
        })
        .collect()
}

fn gray16_pattern(w: u32, h: u32) -> Vec<u16> {
    pattern16((w * h) as usize, 1)
}

fn rgb16_pattern(w: u32, h: u32) -> Vec<u16> {
    pattern16((w * h * 3) as usize, 2)
}

fn dims(w: u32, h: u32) -> Dimensions {
    Dimensions::new(w, h).unwrap()
}

/// Every gamut compression that can carry 16-bit samples, paired with the predictors it allows.
/// CCITT is bilevel-only and so is absent by construction.
const GAMUT_MODES: &[(Compression, Predictor)] = &[
    (Compression::None, Predictor::None),
    (Compression::PackBits, Predictor::None),
    (Compression::Lzw, Predictor::None),
    (Compression::Lzw, Predictor::HorizontalDifferencing),
    (Compression::Deflate, Predictor::None),
    (Compression::Deflate, Predictor::HorizontalDifferencing),
];

fn encoder(order: ByteOrder, compression: Compression, predictor: Predictor) -> TiffEncoder {
    TiffEncoder::new()
        .with_byte_order(order)
        .with_compression(compression)
        .with_predictor(predictor)
}

#[test]
fn gray16_roundtrips_in_gamut() {
    for &(w, h) in SIZES {
        let src = gray16_pattern(w, h);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for &(compression, predictor) in GAMUT_MODES {
                let tiff = encoder(order, compression, predictor)
                    .encode_to_vec(ImageRef::<Gray16>::new(&src, dims(w, h)).unwrap())
                    .expect("encode");
                let got: ImageBuf<Gray16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
                assert_eq!(got.dimensions(), dims(w, h));
                assert_eq!(
                    got.as_samples(),
                    src.as_slice(),
                    "gray16 {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn rgb16_and_rgba16_roundtrip_in_gamut() {
    for &(w, h) in SIZES {
        let rgb = rgb16_pattern(w, h);
        let rgba = pattern16((w * h * 4) as usize, 3);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for &(compression, predictor) in GAMUT_MODES {
                let enc = encoder(order, compression, predictor);
                let tiff = enc
                    .encode_to_vec(ImageRef::<Rgb16>::new(&rgb, dims(w, h)).unwrap())
                    .expect("encode");
                let got: ImageBuf<Rgb16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
                assert_eq!(
                    got.as_samples(),
                    rgb.as_slice(),
                    "rgb16 {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );

                let tiff = enc
                    .encode_to_vec(ImageRef::<Rgba16>::new(&rgba, dims(w, h)).unwrap())
                    .expect("encode");
                let got: ImageBuf<Rgba16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
                assert_eq!(
                    got.as_samples(),
                    rgba.as_slice(),
                    "rgba16 {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn rgb16_roundtrips_in_gamut_as_tiles() {
    // Tiles are predicted independently and the encoder's tiled predictor is Deflate-only, so this
    // covers both the padded-edge blit and the per-tile predictor at 16 bits.
    for &(w, h) in SIZES {
        let src = rgb16_pattern(w, h);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for (compression, predictor) in [
                (Compression::None, Predictor::None),
                (Compression::Lzw, Predictor::None),
                (Compression::Deflate, Predictor::HorizontalDifferencing),
            ] {
                let tiff = encoder(order, compression, predictor)
                    .with_tiling(16, 16)
                    .encode_to_vec(ImageRef::<Rgb16>::new(&src, dims(w, h)).unwrap())
                    .expect("encode");
                let got: ImageBuf<Rgb16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
                assert_eq!(
                    got.as_samples(),
                    src.as_slice(),
                    "rgb16 tiled {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn gamut_sixteen_bit_is_decoded_by_libtiff() {
    // The other direction, and the one that actually pins the encoder's byte order: a gamut-only
    // round-trip would survive a swapped pack/unpack pair, since it writes and reads with the same
    // code. libtiff reading an `MM` file gamut wrote cannot.
    for &(w, h) in SIZES {
        let gray = gray16_pattern(w, h);
        let rgb = rgb16_pattern(w, h);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for &(compression, predictor) in GAMUT_MODES {
                let enc = encoder(order, compression, predictor);

                let tiff = enc
                    .encode_to_vec(ImageRef::<Gray16>::new(&gray, dims(w, h)).unwrap())
                    .expect("encode");
                let oracle = libtiff_oracle::decode_tiff16(&tiff).expect("libtiff decode");
                assert_eq!((oracle.width, oracle.height), (w, h));
                assert_eq!(
                    oracle.samples, gray,
                    "gray16 {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );

                let tiff = enc
                    .encode_to_vec(ImageRef::<Rgb16>::new(&rgb, dims(w, h)).unwrap())
                    .expect("encode");
                let oracle = libtiff_oracle::decode_tiff16(&tiff).expect("libtiff decode");
                assert_eq!(
                    oracle.samples, rgb,
                    "rgb16 {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn gamut_tiled_sixteen_bit_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = rgb16_pattern(w, h);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for (compression, predictor) in [
                (Compression::Lzw, Predictor::None),
                (Compression::Deflate, Predictor::HorizontalDifferencing),
            ] {
                let tiff = encoder(order, compression, predictor)
                    .with_tiling(16, 16)
                    .encode_to_vec(ImageRef::<Rgb16>::new(&src, dims(w, h)).unwrap())
                    .expect("encode");
                let oracle = libtiff_oracle::decode_tiff16(&tiff).expect("libtiff decode");
                assert_eq!(
                    oracle.samples, src,
                    "rgb16 tiled {compression:?} {predictor:?} {order:?} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn sixteen_bit_encode_declares_its_depth_and_alpha() {
    let src = pattern16(4 * 4 * 4, 5);
    let tiff = TiffEncoder::new()
        .encode_to_vec(ImageRef::<Rgba16>::new(&src, dims(4, 4)).unwrap())
        .expect("encode");
    let ifd = &read(&tiff).expect("read").ifds[0];
    assert_eq!(
        ifd.get_u32_vec(tags::BITS_PER_SAMPLE),
        Some(vec![16, 16, 16, 16])
    );
    // Unassociated alpha, matching the 8-bit RGBA impl.
    assert_eq!(ifd.get_u32_vec(tags::EXTRA_SAMPLES), Some(vec![2]));
}

#[test]
fn sixteen_bit_rejects_ccitt_which_is_bilevel_only() {
    let src = gray16_pattern(8, 8);
    let got = TiffEncoder::new()
        .with_compression(Compression::CcittGroup4Fax)
        .encode_to_vec(ImageRef::<Gray16>::new(&src, dims(8, 8)).unwrap());
    assert!(got.is_err(), "CCITT cannot carry 16-bit samples");
}

#[test]
fn info_reports_the_declared_layout() {
    let src = rgb16_pattern(17, 13);
    let tiff = TiffEncoder::new()
        .with_byte_order(ByteOrder::BigEndian)
        .with_compression(Compression::Deflate)
        .with_predictor(Predictor::HorizontalDifferencing)
        .encode_to_vec(ImageRef::<Rgb16>::new(&src, dims(17, 13)).unwrap())
        .expect("encode");

    let info = TiffDecoder::new().info(&tiff).expect("info");
    assert_eq!((info.width, info.height), (17, 13));
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(info.samples_per_pixel, 3);
    assert_eq!(info.sample_format, SampleFormat::UnsignedInteger);
    assert_eq!(info.photometric, PhotometricInterpretation::Rgb);
    assert_eq!(info.compression, Compression::Deflate);
    assert_eq!(info.predictor, Predictor::HorizontalDifferencing);
    assert_eq!(info.byte_order, ByteOrder::BigEndian);
    assert!(!info.tiled);
}

#[test]
fn info_reports_tiling_and_the_eight_bit_default() {
    // An 8-bit tiled page with no SampleFormat tag: `tiled` must be set, and the absent tag must
    // read back as the TIFF 6.0 unsigned-integer default rather than an error.
    let src: Vec<u8> = (0..17 * 13 * 3).map(|i| (i * 7) as u8).collect();
    let tiff = TiffEncoder::new()
        .with_tiling(16, 16)
        .encode_to_vec(ImageRef::<Rgb8>::new(&src, dims(17, 13)).unwrap())
        .expect("encode");
    let info = TiffDecoder::new().info(&tiff).expect("info");
    assert_eq!(info.bits_per_sample, 8);
    assert_eq!(info.sample_format, SampleFormat::UnsignedInteger);
    assert!(info.tiled);
}

#[test]
fn info_describes_a_page_the_decoder_refuses() {
    // The contract that makes the probe worth having: a 32-bit float page is *described*, not
    // rejected, so a caller can dispatch on the depth it finds instead of inferring it from a
    // decode failure. Decoding the same file must still fail.
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![4]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![32]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![1]));
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![2]));
    ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![3]));
    let tiff = write_image(
        ByteOrder::LittleEndian,
        Variant::Classic,
        &ifd,
        &[vec![0u8; 4 * 2 * 4]],
    )
    .expect("write fixture");

    let info = TiffDecoder::new()
        .info(&tiff)
        .expect("a float page must still describe itself");
    assert_eq!(info.bits_per_sample, 32);
    assert_eq!(info.sample_format, SampleFormat::FloatingPoint);

    assert!(
        TiffDecoder::new().decode_page(&tiff, 0).is_err(),
        "describing a float page must not imply decoding it"
    );
}

#[test]
fn info_page_addresses_each_page_and_rejects_an_out_of_range_one() {
    let a: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 3) as u8).collect();
    let b = gray16_pattern(6, 5);
    let mut pages = Vec::new();
    TiffEncoder::new()
        .encode_image(ImageRef::<Rgb8>::new(&a, dims(4, 4)).unwrap(), &mut pages)
        .expect("encode");
    // A genuinely multi-page file needs the multipage writer; here it is enough that page 0 is
    // described and that an out-of-range index is refused the same way `decode_page` refuses it.
    let dec = TiffDecoder::new();
    assert_eq!(dec.info_page(&pages, 0).expect("page 0").bits_per_sample, 8);
    assert!(dec.info_page(&pages, 1).is_err(), "page 1 does not exist");

    let single = TiffEncoder::new()
        .encode_to_vec(ImageRef::<Gray16>::new(&b, dims(6, 5)).unwrap())
        .expect("encode");
    let info = dec.info_page(&single, 0).expect("page 0");
    assert_eq!((info.width, info.height, info.bits_per_sample), (6, 5, 16));
}

#[test]
fn libtiff_gray16_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = gray16_pattern(w, h);
        for big_endian in [false, true] {
            for (compression, predictor) in [
                (OC::None, 1u16),
                (OC::PackBits, 1),
                (OC::Lzw, 1),
                (OC::Lzw, 2),
                (OC::Deflate, 2),
            ] {
                let tiff =
                    libtiff_oracle::encode_gray16(&src, w, h, compression, predictor, big_endian)
                        .expect("libtiff encode");
                let got: ImageBuf<Gray16> = TiffDecoder::new()
                    .decode_image(&tiff)
                    .expect("gamut decode");
                assert_eq!(got.dimensions(), dims(w, h));
                assert_eq!(
                    got.as_samples(),
                    src.as_slice(),
                    "gray16 {compression:?} p{predictor} be={big_endian} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn libtiff_rgb16_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = rgb16_pattern(w, h);
        for big_endian in [false, true] {
            for (compression, predictor) in [
                (OC::None, 1u16),
                (OC::PackBits, 1),
                (OC::Lzw, 1),
                (OC::Lzw, 2),
                (OC::Deflate, 2),
            ] {
                let tiff =
                    libtiff_oracle::encode_rgb16(&src, w, h, compression, predictor, big_endian)
                        .expect("libtiff encode");
                let got: ImageBuf<Rgb16> = TiffDecoder::new()
                    .decode_image(&tiff)
                    .expect("gamut decode");
                assert_eq!(got.dimensions(), dims(w, h));
                assert_eq!(
                    got.as_samples(),
                    src.as_slice(),
                    "rgb16 {compression:?} p{predictor} be={big_endian} at {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn libtiff_rgb16_tiled_is_decoded_by_gamut() {
    // Tiles are predicted independently, and every non-multiple-of-16 size leaves padded right and
    // bottom edges to crop — both are places a widened byte stride can go wrong silently.
    for &(w, h) in SIZES {
        let src = rgb16_pattern(w, h);
        for (compression, predictor) in [(OC::None, 1u16), (OC::Lzw, 1), (OC::Deflate, 2)] {
            let tiff =
                libtiff_oracle::encode_rgb16_tiled(&src, w, h, 16, 16, compression, predictor)
                    .expect("libtiff encode");
            let got: ImageBuf<Rgb16> = TiffDecoder::new()
                .decode_image(&tiff)
                .expect("gamut decode");
            assert_eq!(got.dimensions(), dims(w, h));
            assert_eq!(
                got.as_samples(),
                src.as_slice(),
                "rgb16 tiled {compression:?} p{predictor} at {w}x{h}"
            );
        }
    }
}

#[test]
fn big_endian_sixteen_bit_is_not_byte_swapped() {
    // The endianness pin. A gamut-only round-trip is symmetric — it writes and reads with the same
    // code, so a swapped load paired with a swapped store cancels out and the test still passes.
    // Only an independently produced `MM` file catches it, and the asserted value is exact.
    let src: Vec<u16> = vec![0x0102, 0xFFFE, 0x00FF, 0xFF00];
    let tiff =
        libtiff_oracle::encode_gray16(&src, 4, 1, OC::None, 1, true).expect("libtiff encode");
    assert_eq!(
        &tiff[..2],
        b"MM",
        "the oracle must have written a big-endian file"
    );
    let got: ImageBuf<Gray16> = TiffDecoder::new()
        .decode_image(&tiff)
        .expect("gamut decode");
    assert_eq!(got.as_samples(), src.as_slice());
}

#[test]
fn eight_bit_widens_to_sixteen_by_257() {
    // x257 is exact at both ends (0 -> 0, 255 -> 65535) and is the inverse of the >> 8 narrowing
    // below. Asserting the values rather than a round-trip is what distinguishes it from x256.
    let src: Vec<u8> = vec![0, 1, 128, 255];
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_image(ImageRef::<Gray8>::new(&src, dims(4, 1)).unwrap(), &mut tiff)
        .expect("encode");
    let got: ImageBuf<Gray16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    assert_eq!(got.as_samples(), &[0u16, 257, 32896, 65535]);
}

#[test]
fn sixteen_bit_narrows_to_eight_only_under_a_depth_policy() {
    // Narrowing is a loss, so the default lossless decoder refuses it by name; a `DepthPolicy`
    // opts in. The values then come from the shared engine's rescaling — round(v * 255 / 65535),
    // half up — not a truncating shift, so 0x01FF is 0x02 and 0xFFFF stays 0xFF.
    let src: Vec<u16> = vec![0x0000, 0x00FF, 0x0100, 0x01FF, 0xFFFF];
    let tiff = libtiff_oracle::encode_gray16(&src, 5, 1, OC::None, 1, false).expect("libtiff");

    let refused: Result<ImageBuf<Gray8>, _> = TiffDecoder::new().decode_image(&tiff);
    assert_eq!(
        refused.err().map(|e| e.static_message()),
        Some(Some(
            "convert: target layout is narrower than the source; set a DepthPolicy"
        )),
        "the default decoder refuses to narrow"
    );

    let dec = TiffDecoder::new().with_convert_policy(ConvertPolicy::permissive());
    let got: ImageBuf<Gray8> = dec.decode_image(&tiff).expect("decode");
    assert_eq!(got.as_samples(), &[0x00u8, 0x01, 0x01, 0x02, 0xFF]);
}

#[test]
fn sixteen_bit_rgb_narrows_through_the_eight_bit_surface() {
    // `decode_page` and `DecodeImage<Rgb8>` both reach 8-bit RGB from a 16-bit page, and both do
    // so only once the policy permits the narrowing.
    let (w, h) = (17u32, 13u32);
    let src = rgb16_pattern(w, h);
    let tiff = libtiff_oracle::encode_rgb16(&src, w, h, OC::Lzw, 1, false).expect("libtiff");
    let expected: Vec<u8> = src
        .iter()
        .map(|&v| ((u32::from(v) * 255 + 32767) / 65535) as u8)
        .collect();

    assert!(
        DecodeImage::<Rgb8>::decode_image(&TiffDecoder::new(), &tiff).is_err(),
        "the default decoder refuses to narrow"
    );

    let dec = TiffDecoder::new().with_convert_policy(ConvertPolicy::permissive());
    let got: ImageBuf<Rgb8> = dec.decode_image(&tiff).expect("decode");
    assert_eq!(got.as_samples(), expected.as_slice());
    let page = dec.decode_page(&tiff, 0).expect("decode_page");
    assert_eq!(page.as_samples(), expected.as_slice());
}

#[test]
fn gray16_replicates_and_gains_opaque_alpha() {
    // Channel widening is orthogonal to sample widening: a single-sample 16-bit page must present
    // as RGB16 by replication and as RGBA16 with a full-range opaque alpha (not 255).
    let src: Vec<u16> = vec![0x1234, 0xABCD];
    let tiff = libtiff_oracle::encode_gray16(&src, 2, 1, OC::None, 1, false).expect("libtiff");

    let rgb: ImageBuf<Rgb16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    assert_eq!(
        rgb.as_samples(),
        &[0x1234, 0x1234, 0x1234, 0xABCD, 0xABCD, 0xABCD]
    );

    let rgba: ImageBuf<Rgba16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    assert_eq!(
        rgba.as_samples(),
        &[
            0x1234, 0x1234, 0x1234, 0xFFFF, 0xABCD, 0xABCD, 0xABCD, 0xFFFF
        ]
    );
}

#[test]
fn single_sample_pixel_types_reject_a_multi_sample_page() {
    // The assertion is on the *message*, not `is_err()`. Without the layout guard these decode
    // calls still fail — `ImageBuf::new` rejects a buffer whose length does not match the
    // dimensions — so only the message distinguishes the guard from that incidental backstop.
    // Since presentation moved to the shared engine, the message names the missing *policy*: a
    // colour page reaches a grey layout only through a `LumaPolicy`, and never reaches CMYK.
    let rgb = rgb16_pattern(4, 4);
    let tiff = libtiff_oracle::encode_rgb16(&rgb, 4, 4, OC::None, 1, false).expect("libtiff");

    let dec = TiffDecoder::new();
    let gray16: Result<ImageBuf<Gray16>, _> = dec.decode_image(&tiff);
    assert_eq!(
        gray16.err().map(|e| e.static_message()),
        Some(Some(
            "convert: target layout cannot hold colour; set a LumaPolicy"
        )),
        "an RGB page cannot be presented as Gray16"
    );
    let gray8: Result<ImageBuf<Gray8>, _> = dec.decode_image(&tiff);
    assert_eq!(
        gray8.err().map(|e| e.static_message()),
        Some(Some(
            "convert: target layout cannot hold colour; set a LumaPolicy"
        ))
    );
    let cmyk: Result<ImageBuf<Cmyk8>, _> = dec.decode_image(&tiff);
    assert_eq!(
        cmyk.err().map(|e| e.static_message()),
        Some(Some(
            "convert: CMYK conversion needs a colour-management transform, not a layout change"
        )),
        "a 3-sample page cannot be presented as CMYK"
    );
}

#[test]
fn white_is_zero_inverts_at_sixteen_bits() {
    // The 16-bit sibling of the 8-bit WhiteIsZero test. libtiff writes MINISWHITE faithfully, so
    // the same stored samples must come back inverted against the full 16-bit range — the only
    // thing pinning `u16::MAX - v` rather than a stray `255 - v`.
    let src: Vec<u16> = vec![0x0000, 0x1234, 0xFFFF, 0x00FF];
    let tiff = libtiff_oracle::encode_gray16_miniswhite(&src, 4, 1, OC::None).expect("libtiff");
    let got: ImageBuf<Gray16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    let inverted: Vec<u16> = src.iter().map(|&v| u16::MAX - v).collect();
    assert_eq!(got.as_samples(), inverted.as_slice());
}

#[test]
fn sixteen_bit_cmyk_reaches_eight_bit_ink_only_under_a_depth_policy() {
    // A 16-bit CMYK page is the one decodable layout with no pixel type to land in: the ink model
    // has no 16-bit member. It must still decode — to `Cmyk8`, and only once the policy permits the
    // narrowing — rather than being refused for want of a `Cmyk16`. Hand-built because gamut's
    // encoder writes 8-bit CMYK only.
    let samples: Vec<u16> = vec![
        0x0000, 0x00FF, 0x0100, 0x01FF, // pixel 0
        0xFFFF, 0x8000, 0x4321, 0xFEDC, // pixel 1
    ];
    let mut strip = Vec::with_capacity(samples.len() * 2);
    for &v in &samples {
        strip.extend_from_slice(&v.to_le_bytes());
    }
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![16, 16, 16, 16]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![4]));
    ifd.set(
        tags::PHOTOMETRIC_INTERPRETATION,
        Value::Short(vec![PhotometricInterpretation::Cmyk as u16]),
    );
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
    let tiff = write_image(ByteOrder::LittleEndian, Variant::Classic, &ifd, &[strip])
        .expect("write fixture");

    let refused: Result<ImageBuf<Cmyk8>, _> = TiffDecoder::new().decode_image(&tiff);
    assert_eq!(
        refused.err().map(|e| e.static_message()),
        Some(Some(
            "TIFF: 16-bit CMYK needs a depth policy to reach 8-bit ink samples"
        )),
        "the default decoder refuses to narrow ink samples"
    );

    let dec = TiffDecoder::new().with_convert_policy(ConvertPolicy::permissive());
    let got: ImageBuf<Cmyk8> = dec.decode_image(&tiff).expect("decode");
    let expected: Vec<u8> = samples
        .iter()
        .map(|&v| ((u32::from(v) * 255 + 32767) / 65535) as u8)
        .collect();
    assert_eq!(got.as_samples(), expected.as_slice());
    assert_eq!(got.dimensions(), dims(2, 1));
}
