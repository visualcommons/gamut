//! Horizontal differencing predictor (LZW + Predictor=2): tier-1 + libtiff cross-checks (P10).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, Predictor, TiffDecoder, TiffEncoder};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (17, 13), (64, 40), (200, 9)];

/// A smooth gradient — the kind of content horizontal differencing helps — with a little texture.
fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(2).wrapping_add(y)) as u8);
            v.push((x.wrapping_add(y.wrapping_mul(2))) as u8);
            v.push((128 + (x as i32 - y as i32)) as u8);
        }
    }
    v
}

fn gray_pattern(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).map(|i| (i / 2) as u8).collect()
}

#[test]
fn predictor_rgb_roundtrips_in_gamut() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::HorizontalDifferencing)
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        let got: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
        assert_eq!(got.as_samples(), src.as_slice(), "{w}x{h}");
    }
}

#[test]
fn gamut_predictor_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let rgb = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::HorizontalDifferencing)
            .encode_image(
                ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        // libtiff reverses the predictor on read, so the decoded samples must match the source.
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!(dec.pixels, rgb, "rgb {w}x{h}");

        let gray = gray_pattern(w, h);
        let mut gtiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::HorizontalDifferencing)
            .encode_image(
                ImageRef::<Gray8>::new(&gray, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut gtiff,
            )
            .expect("encode");
        assert_eq!(
            libtiff_oracle::decode_tiff(&gtiff).expect("decode").pixels,
            gray
        );
    }
}

#[test]
fn libtiff_predictor_is_decoded_by_gamut() {
    use libtiff_oracle::Compression as OC;
    for &(w, h) in SIZES {
        let rgb = rgb_pattern(w, h);
        let tiff =
            libtiff_oracle::encode_rgb8_predictor(&rgb, w, h, OC::Lzw).expect("libtiff encode");
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(got.as_samples(), rgb.as_slice(), "rgb {w}x{h}");

        let gray = gray_pattern(w, h);
        let gtiff =
            libtiff_oracle::encode_gray8_predictor(&gray, w, h, OC::Lzw).expect("libtiff encode");
        let gout: ImageBuf<Gray8> = TiffDecoder::new()
            .decode_image(&gtiff)
            .expect("gamut decode");
        assert_eq!(gout.as_samples(), gray.as_slice(), "gray {w}x{h}");
    }
}
