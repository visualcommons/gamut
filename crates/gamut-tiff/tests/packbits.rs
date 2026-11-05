//! PackBits compression: tier-1 round-trips and libtiff cross-checks (P5).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (16, 16), (17, 13), (64, 100), (130, 9)];

/// A pattern with long horizontal runs (to exercise PackBits replicate runs) plus some variation.
fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let band = (x / 5) as u8; // runs of 5 identical pixels
            v.push(band.wrapping_mul(20));
            v.push((y as u8).wrapping_mul(3));
            v.push(if x % 11 == 0 { 255 } else { band });
        }
    }
    v
}

fn gray_pattern(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).map(|i| ((i / 4) % 6) as u8 * 40).collect()
}

#[test]
fn packbits_rgb_roundtrips_in_gamut() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::PackBits)
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
fn gamut_packbits_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let rgb = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::PackBits)
            .encode_image(
                ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, rgb, "rgb {w}x{h}");

        let gray = gray_pattern(w, h);
        let mut gtiff = Vec::new();
        TiffEncoder::new()
            .with_compression(Compression::PackBits)
            .encode_image(
                ImageRef::<Gray8>::new(&gray, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut gtiff,
            )
            .expect("encode");
        let gdec = libtiff_oracle::decode_tiff(&gtiff).expect("libtiff decode");
        assert_eq!(gdec.pixels, gray, "gray {w}x{h}");
    }
}

#[test]
fn libtiff_packbits_is_decoded_by_gamut() {
    use libtiff_oracle::Compression as OC;
    for &(w, h) in SIZES {
        let rgb = rgb_pattern(w, h);
        let tiff = libtiff_oracle::encode_rgb8(&rgb, w, h, OC::PackBits).expect("libtiff encode");
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(got.as_samples(), rgb.as_slice(), "rgb {w}x{h}");

        let gray = gray_pattern(w, h);
        let gtiff =
            libtiff_oracle::encode_gray8(&gray, w, h, OC::PackBits).expect("libtiff encode");
        let gout: ImageBuf<Gray8> = TiffDecoder::new()
            .decode_image(&gtiff)
            .expect("gamut decode");
        assert_eq!(gout.as_samples(), gray.as_slice(), "gray {w}x{h}");
    }
}
