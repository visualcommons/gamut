//! Multi-page documents: tier-1 + libtiff cross-checks (P17).

use gamut_core::{Dimensions, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

/// Distinct RGB pages of differing dimensions.
fn page(seed: u32, w: u32, h: u32) -> Vec<u8> {
    (0..w * h * 3)
        .map(|i| (i.wrapping_mul(31).wrapping_add(seed.wrapping_mul(97))) as u8)
        .collect()
}

fn pages() -> Vec<(Vec<u8>, u32, u32)> {
    vec![
        (page(0, 8, 6), 8, 6),
        (page(1, 17, 13), 17, 13),
        (page(2, 32, 5), 32, 5),
    ]
}

#[test]
fn multipage_roundtrips_in_gamut() {
    let p = pages();
    let refs: Vec<ImageRef<'_, Rgb8>> = p
        .iter()
        .map(|(d, w, h)| {
            ImageRef::<Rgb8>::new(d.as_slice(), Dimensions::new(*w, *h).unwrap()).unwrap()
        })
        .collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::Lzw)
        .encode_pages_rgb8(&refs, &mut tiff)
        .expect("encode");

    let dec = TiffDecoder::new();
    assert_eq!(dec.page_count(&tiff).expect("count"), p.len());
    for (i, (data, w, h)) in p.iter().enumerate() {
        let got: ImageBuf<Rgb8> = dec.decode_page(&tiff, i).expect("decode page");
        assert_eq!(
            (got.dimensions().width(), got.dimensions().height()),
            (*w, *h)
        );
        assert_eq!(got.as_samples(), data.as_slice(), "page {i}");
    }
    // Out-of-range page is rejected.
    assert!(dec.decode_page(&tiff, p.len()).is_err());
}

#[test]
fn gamut_multipage_is_decoded_by_libtiff() {
    let p = pages();
    let refs: Vec<ImageRef<'_, Rgb8>> = p
        .iter()
        .map(|(d, w, h)| {
            ImageRef::<Rgb8>::new(d.as_slice(), Dimensions::new(*w, *h).unwrap()).unwrap()
        })
        .collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_pages_rgb8(&refs, &mut tiff)
        .expect("encode");
    for (i, (data, w, h)) in p.iter().enumerate() {
        let dec = libtiff_oracle::decode_page(&tiff, i as u32).expect("libtiff decode page");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (*w, *h, 3));
        assert_eq!(&dec.pixels, data, "page {i}");
    }
}

#[test]
fn libtiff_multipage_is_decoded_by_gamut() {
    let p = pages();
    let refs: Vec<(&[u8], u32, u32)> = p.iter().map(|(d, w, h)| (d.as_slice(), *w, *h)).collect();
    let tiff = libtiff_oracle::encode_pages_rgb8(&refs, libtiff_oracle::Compression::Lzw)
        .expect("libtiff encode");
    let dec = TiffDecoder::new();
    assert_eq!(dec.page_count(&tiff).expect("count"), p.len());
    for (i, (data, _, _)) in p.iter().enumerate() {
        let got: ImageBuf<Rgb8> = dec.decode_page(&tiff, i).expect("gamut decode page");
        assert_eq!(got.as_samples(), data.as_slice(), "page {i}");
    }
}
