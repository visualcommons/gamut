//! RGBA images (4 samples, unassociated alpha): tier-1 + libtiff cross-checks (P13).

use gamut_core::convert::{AlphaPolicy, ConvertPolicy};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, ImageBuf, ImageRef, Rgb8, Rgba8,
};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (17, 13), (64, 40)];

fn rgba_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(5).wrapping_add(y)) as u8);
            v.push((y.wrapping_mul(7) ^ x) as u8);
            v.push((x.wrapping_add(y).wrapping_mul(3)) as u8);
            v.push((x.wrapping_mul(y)) as u8); // varied alpha
        }
    }
    v
}

#[test]
fn rgba_roundtrips_in_gamut() {
    for &comp in &[Compression::None, Compression::PackBits, Compression::Lzw] {
        for &(w, h) in SIZES {
            let src = rgba_pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .encode_image(
                    ImageRef::<Rgba8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let got: ImageBuf<Rgba8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h)
            );
            assert_eq!(got.as_samples(), src.as_slice(), "{comp:?} {w}x{h}");

            // An RGB view of an RGBA file discards the alpha channel, so the default (lossless)
            // decoder refuses it rather than deciding for the caller.
            let refused = DecodeImage::<Rgb8>::decode_image(&TiffDecoder::new(), &tiff)
                .expect_err("alpha drop must not happen silently");
            assert_eq!(refused.kind(), ErrorKind::Unsupported);

            // With the loss opted into, the samples pass through unchanged.
            let rgb: ImageBuf<Rgb8> = TiffDecoder::new()
                .convert_policy(ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop))
                .decode_image(&tiff)
                .expect("rgb");
            let expect_rgb: Vec<u8> = src
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            assert_eq!(rgb.as_samples(), expect_rgb.as_slice());
        }
    }
}

#[test]
fn gamut_rgba_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = rgba_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .encode_image(
                ImageRef::<Rgba8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 4));
        assert_eq!(dec.pixels, src, "{w}x{h}");
    }
}

#[test]
fn libtiff_rgba_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = rgba_pattern(w, h);
        let tiff = libtiff_oracle::encode_rgba8(&src, w, h, libtiff_oracle::Compression::Lzw)
            .expect("libtiff encode");
        let got: ImageBuf<Rgba8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(got.as_samples(), src.as_slice(), "{w}x{h}");
    }
}

#[test]
fn grayscale_is_presented_as_replicated_opaque_rgba() {
    // A 1-sample (grayscale) image decoded as RGBA replicates luma into RGB and sets alpha 255.
    use gamut_core::Gray8;
    let (w, h) = (5u32, 4u32);
    let gray: Vec<u8> = (0..w * h).map(|i| (i * 11) as u8).collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_image(
            ImageRef::<Gray8>::new(&gray, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut tiff,
        )
        .unwrap();
    let rgba: ImageBuf<Rgba8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    let s = rgba.as_samples();
    assert_eq!(s.len(), (w * h * 4) as usize);
    for (i, &g) in gray.iter().enumerate() {
        assert_eq!(&s[i * 4..i * 4 + 4], &[g, g, g, 255], "pixel {i}");
    }
}

#[test]
fn rgb_is_presented_as_opaque_rgba() {
    // A 3-sample (RGB) image decoded as RGBA passes RGB through and sets alpha 255.
    let (w, h) = (5u32, 4u32);
    let src: Vec<u8> = (0..w * h * 3).map(|i| (i * 13) as u8).collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut tiff,
        )
        .unwrap();
    let rgba: ImageBuf<Rgba8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    let s = rgba.as_samples();
    for i in 0..(w * h) as usize {
        assert_eq!(
            &s[i * 4..i * 4 + 4],
            &[src[i * 3], src[i * 3 + 1], src[i * 3 + 2], 255],
            "pixel {i}"
        );
    }
}
