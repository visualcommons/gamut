//! Differential conformance cross-checks against a vendored libtiff (P4).
//!
//! Two directions, pixel-exact for the lossless uncompressed path:
//!   A. gamut encodes → libtiff decodes back to the source pixels;
//!   B. libtiff encodes → gamut decodes back to the source pixels.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{TiffDecoder, TiffEncoder};
use libtiff_oracle::Compression;

mod common;

use common::{gray_pattern, rgb_pattern};

const SIZES: &[(u32, u32)] = &[(1, 1), (3, 7), (16, 16), (17, 13), (64, 100), (100, 70)];

#[test]
fn gamut_rgb_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("gamut encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, src, "RGB mismatch at {w}x{h}");
    }
}

#[test]
fn gamut_gray_is_decoded_by_libtiff() {
    for &(w, h) in SIZES {
        let src = gray_pattern(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .encode_image(
                ImageRef::<Gray8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut tiff,
            )
            .expect("gamut encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 1));
        assert_eq!(dec.pixels, src, "gray mismatch at {w}x{h}");
    }
}

#[test]
fn libtiff_rgb_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let tiff =
            libtiff_oracle::encode_rgb8(&src, w, h, Compression::None).expect("libtiff encode");
        let got: ImageBuf<Rgb8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (w, h)
        );
        assert_eq!(got.as_samples(), src.as_slice(), "RGB mismatch at {w}x{h}");
    }
}

#[test]
fn libtiff_gray_is_decoded_by_gamut() {
    for &(w, h) in SIZES {
        let src = gray_pattern(w, h);
        let tiff =
            libtiff_oracle::encode_gray8(&src, w, h, Compression::None).expect("libtiff encode");
        let got: ImageBuf<Gray8> = TiffDecoder::new()
            .decode_image(&tiff)
            .expect("gamut decode");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (w, h)
        );
        assert_eq!(got.as_samples(), src.as_slice(), "gray mismatch at {w}x{h}");
    }
}

/// A libtiff-written file under each of the four codes libtiff's `tiff.h` assigns beyond the
/// specifications: `info` names the scheme, and decoding its data stays `Unsupported`.
#[test]
fn libtiff_only_compression_is_named_by_info_and_refused_by_decode() {
    for (code, scheme) in [
        (50000, gamut_tiff::Compression::Zstd),
        (50001, gamut_tiff::Compression::Webp),
        (34712, gamut_tiff::Compression::Jp2000),
        (34887, gamut_tiff::Compression::Lerc),
    ] {
        let tiff = libtiff_oracle::encode_gray8_raw_strip(4, 2, code, &[0; 8])
            .expect("libtiff writes the code");
        let dec = TiffDecoder::new();
        assert_eq!(dec.info(&tiff).expect("info").compression, scheme, "{code}");
        let err = DecodeImage::<Gray8>::decode_image(&dec, &tiff).expect_err("decoded");
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported, "{code}");
    }
}
