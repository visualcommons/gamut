//! CMYK (separated) images: tier-1 + libtiff cross-checks (P14).

use gamut_core::{Cmyk8, DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (17, 13), (64, 40)];

fn cmyk_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(9)) as u8);
            v.push((y.wrapping_mul(11)) as u8);
            v.push((x ^ y) as u8);
            v.push((x.wrapping_add(y)) as u8);
        }
    }
    v
}

#[test]
fn cmyk_roundtrips_in_gamut() {
    for &comp in &[Compression::None, Compression::PackBits, Compression::Lzw] {
        for &(w, h) in SIZES {
            let src = cmyk_pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .encode_image(
                    ImageRef::<Cmyk8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let got: ImageBuf<Cmyk8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h)
            );
            assert_eq!(got.as_samples(), src.as_slice(), "{comp:?} {w}x{h}");
        }
    }
}

#[test]
fn gamut_cmyk_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = cmyk_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::Lzw)
            .encode_image(
                ImageRef::<Cmyk8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 4));
        assert_eq!(dec.pixels, src, "{w}x{h}");
    }
}

#[test]
fn libtiff_cmyk_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = cmyk_pattern(w, h);
        let tiff = libtiff_oracle::encode_cmyk8(&src, w, h, libtiff_oracle::Compression::Lzw)
            .expect("libtiff encode");
        let got: ImageBuf<Cmyk8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(got.as_samples(), src.as_slice(), "{w}x{h}");
    }
}
