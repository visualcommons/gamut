//! 1-bit bilevel images: tier-1 round-trips and libtiff cross-checks (P5b).

use gamut_core::{Bilevel, DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

// Widths deliberately not multiples of 8 exercise the row bit-padding.
const SIZES: &[(u32, u32)] = &[(1, 1), (5, 4), (7, 7), (13, 9), (17, 16), (200, 50)];

/// A bilevel pattern (bytes 0/255) with short runs, to exercise bit packing and PackBits.
fn pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let white = ((x / 3) + y) % 2 == 0;
            v.push(if white { 255 } else { 0 });
        }
    }
    v
}

#[test]
fn bilevel_roundtrips_in_gamut() {
    for &comp in &[Compression::None, Compression::PackBits] {
        for &(w, h) in SIZES {
            let src = pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .encode_image(
                    ImageRef::<Bilevel>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (got.dimensions().width(), got.dimensions().height()),
                (w, h)
            );
            assert_eq!(got.as_samples(), src.as_slice(), "{comp:?} {w}x{h}");
        }
    }
}

#[test]
fn gamut_bilevel_is_decoded_by_libtiff() {
    for &comp in &[Compression::None, Compression::PackBits] {
        for &(w, h) in SIZES {
            let src = pattern(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .encode_image(
                    ImageRef::<Bilevel>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
            assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 1));
            assert_eq!(dec.pixels, src, "{comp:?} {w}x{h}");
        }
    }
}

#[test]
fn libtiff_bilevel_is_decoded_by_gamut() {
    use libtiff_oracle::Compression as OC;
    for &comp in &[OC::None, OC::PackBits] {
        for &(w, h) in SIZES {
            let src = pattern(w, h);
            let tiff = libtiff_oracle::encode_bilevel(&src, w, h, comp).expect("libtiff encode");
            let got: ImageBuf<Gray8> = TiffDecoder::new()
                .decode_image(&tiff)
                .expect("gamut decode");
            assert_eq!(got.as_samples(), src.as_slice(), "{comp:?} {w}x{h}");
        }
    }
}
