//! Differential cross-check: libtiff still reads a gamut TIFF that carries embedded metadata.
//!
//! The seam adds XMP (700), IPTC/NAA (33723), ICC (34675) and an `ExifIFD` (34665) to IFD 0.
//! Those are out-of-line values, so they move the pixel data's offsets — the risk this file
//! exists for is a directory whose metadata is well-formed but whose strip offsets no longer
//! point where the pixels are. libtiff, not gamut's own reader, is the judge.

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_tiff::{Ifd, TiffEncoder, TiffMetadata, Value};

mod common;

use common::rgb_pattern;

const SIZES: &[(u32, u32)] = &[(1, 1), (17, 13), (64, 100)];

/// A metadata set whose blocks are large enough to be stored out of line (past the 4-byte inline
/// threshold), which is what makes them displace the pixel data.
fn metadata() -> TiffMetadata {
    let mut exif = Ifd::new();
    exif.set(33434, Value::Rational(vec![(1, 250)])); // ExposureTime
    TiffMetadata::new()
        .with_xmp(b"<x:xmpmeta><rdf:RDF/></x:xmpmeta>".to_vec())
        .with_iptc(vec![0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'])
        .with_icc(vec![0, 0, 0, 12, b'a', b'c', b's', b'p', 1, 2, 3, 4])
        .with_exif(exif)
}

#[test]
fn libtiff_decodes_a_gamut_rgb_image_carrying_metadata() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let tiff = TiffEncoder::new()
            .with_metadata(metadata())
            .encode_to_vec(
                ImageRef::<Rgb8>::new(
                    &src,
                    Dimensions {
                        width: w,
                        height: h,
                    },
                )
                .expect("image"),
            )
            .expect("gamut encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, src, "RGB mismatch at {w}x{h} with metadata");
    }
}
