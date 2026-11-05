//! End-to-end pixel round-trips for the uncompressed baseline path (P3, the keystone).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{ByteOrder, Ifd, TiffDecoder, TiffEncoder, Value, Variant, tags, write_image};

const SIZES: &[(u32, u32)] = &[
    (1, 1),
    (2, 2),
    (7, 1),
    (1, 9),
    (17, 13),
    (32, 32),
    (64, 100),
];

fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(7) ^ y) as u8);
            v.push((x + y * 3) as u8);
            v.push((x.wrapping_mul(y).wrapping_add(11)) as u8);
        }
    }
    v
}

fn gray_pattern(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .map(|i| (i.wrapping_mul(2531) >> 3) as u8)
        .collect()
}

#[test]
fn rgb_roundtrips_all_sizes_both_orders() {
    for &(w, h) in SIZES {
        let dims = Dimensions::new(w, h).unwrap();
        let src = rgb_pattern(w, h);
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let mut tiff = Vec::new();
            let n = TiffEncoder::new()
                .with_byte_order(order)
                .encode_image(ImageRef::<Rgb8>::new(&src, dims).unwrap(), &mut tiff)
                .expect("encode");
            assert_eq!(n, tiff.len());

            let got: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h),
                "dims {w}x{h} {order:?}"
            );
            assert_eq!(got.as_samples(), src.as_slice(), "pixels {w}x{h} {order:?}");
        }
    }
}

#[test]
fn gray_roundtrips_and_replicates_to_rgb() {
    for &(w, h) in SIZES {
        let dims = Dimensions::new(w, h).unwrap();
        let src = gray_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .encode_image(ImageRef::<Gray8>::new(&src, dims).unwrap(), &mut tiff)
            .expect("encode");

        let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("gray");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (w, h)
        );
        assert_eq!(got.as_samples(), src.as_slice());

        let rgb: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("rgb");
        let rgb = rgb.as_samples();
        for (i, &v) in src.iter().enumerate() {
            assert_eq!(&rgb[i * 3..i * 3 + 3], &[v, v, v]);
        }
    }
}

#[test]
fn multi_strip_image_is_split_and_reassembled() {
    // 64x100 RGB => row_bytes 192, ~42 rows/strip => 3 strips, exercising strip assembly.
    let (w, h) = (64u32, 100u32);
    let src = rgb_pattern(w, h);
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut tiff,
        )
        .expect("encode");
    let parsed = gamut_tiff::read(&tiff).expect("parse");
    let offs = parsed.ifds[0]
        .get_u32_vec(tags::STRIP_OFFSETS)
        .expect("offsets");
    assert!(
        offs.len() > 1,
        "expected multiple strips, got {}",
        offs.len()
    );

    let got: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    assert_eq!(got.as_samples(), src.as_slice());
}

#[test]
fn white_is_zero_is_inverted_on_decode() {
    // Hand-build a 1x2 WhiteIsZero grayscale image: stored [0, 255] => decoded [255, 0].
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![0])); // WhiteIsZero
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
    let tiff = write_image(
        ByteOrder::LittleEndian,
        Variant::Classic,
        &ifd,
        &[vec![0u8, 255u8]],
    )
    .expect("write");

    let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    assert_eq!(got.as_samples(), &[255, 0]);
}
