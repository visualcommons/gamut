//! Tiled images: tier-1 round-trips and libtiff cross-checks (P12).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

// Dimensions that don't divide the 16×16 tiles, exercising edge-tile padding.
const SIZES: &[(u32, u32)] = &[(16, 16), (17, 13), (40, 33), (64, 48), (100, 70)];

fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(5).wrapping_add(y)) as u8);
            v.push((y.wrapping_mul(7) ^ x) as u8);
            v.push((x.wrapping_add(y).wrapping_mul(3)) as u8);
        }
    }
    v
}

fn gray_pattern(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .map(|i| (i.wrapping_mul(53) >> 2) as u8)
        .collect()
}

#[test]
fn tiled_roundtrips_in_gamut() {
    for &comp in &[
        Compression::None,
        Compression::PackBits,
        Compression::Lzw,
        Compression::Deflate,
    ] {
        for &(w, h) in SIZES {
            let src = rgb_pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .with_tiling(16, 16)
                .encode_image(
                    ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let got: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h)
            );
            assert_eq!(got.as_samples(), src.as_slice(), "{comp:?} {w}x{h}");

            let gray = gray_pattern(w, h);
            let mut gtiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .with_tiling(32, 16)
                .encode_image(
                    ImageRef::<Gray8>::new(&gray, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut gtiff,
                )
                .expect("encode");
            let gout: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&gtiff).expect("decode");
            assert_eq!(gout.as_samples(), gray.as_slice(), "gray {comp:?} {w}x{h}");
        }
    }
}

#[test]
fn gamut_tiled_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .with_tiling(16, 16)
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        // libtiff's RGBA reader handles tiled images and crops padding.
        let (rw, rh, rgba) = libtiff_oracle::decode_rgba(&tiff).expect("libtiff rgba");
        assert_eq!((rw, rh), (w, h));
        let got: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        assert_eq!(got, src, "{w}x{h}");
    }
}

#[test]
fn libtiff_tiled_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let tiff =
            libtiff_oracle::encode_rgb8_tiled(&src, w, h, 16, 16, libtiff_oracle::Compression::Lzw)
                .expect("libtiff encode");
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(got.as_samples(), src.as_slice(), "{w}x{h}");
    }
}
