//! Multi-page documents: tier-1 + libtiff cross-checks (P17).

use gamut_core::{Bilevel, Dimensions, Gray16, GrayAlpha8, ImageBuf, ImageRef, Rgb8, Rgba16};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder, TiffPixel};

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
        .encode_pages(&refs, &mut tiff)
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
        .encode_pages(&refs, &mut tiff)
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

/// Two pages of layout `P` — 7x3 and 16x2, so a sub-byte row pads and a whole-byte one does not —
/// filled from `sample`, encoded with `encode_pages` and returned beside the file.
fn two_pages<P: TiffPixel>(sample: fn(usize) -> P::Sample) -> (Vec<Vec<P::Sample>>, Vec<u8>) {
    let shapes = [(7u32, 3u32), (16, 2)];
    let data: Vec<Vec<P::Sample>> = shapes
        .iter()
        .enumerate()
        .map(|(page, &(w, h))| {
            (0..(w * h) as usize * P::CHANNELS)
                .map(|i| sample(i + page * 101))
                .collect()
        })
        .collect();
    let refs: Vec<ImageRef<'_, P>> = data
        .iter()
        .zip(shapes)
        .map(|(d, (w, h))| ImageRef::new(d, Dimensions::new(w, h).unwrap()).unwrap())
        .collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::Deflate)
        .encode_pages(&refs, &mut tiff)
        .expect("encode");
    (data, tiff)
}

/// `encode_pages` is pixel-generic: each layout's pages read back as written, page by page.
#[test]
fn pages_of_any_layout_roundtrip_in_gamut() {
    fn check<P: TiffPixel>(sample: fn(usize) -> P::Sample) {
        let (data, tiff) = two_pages::<P>(sample);
        let dec = TiffDecoder::new();
        assert_eq!(dec.page_count(&tiff).unwrap(), 2);
        for (i, page) in data.iter().enumerate() {
            let got: ImageBuf<P> = dec.decode_page_as(&tiff, i).expect("decode page");
            assert_eq!(
                got.as_samples(),
                page.as_slice(),
                "{:?} page {i}",
                P::FORMAT
            );
        }
    }
    check::<Gray16>(|i| (i as u16).wrapping_mul(40_503));
    check::<Rgba16>(|i| (i as u16).wrapping_mul(977) ^ 0x5a5a);
    check::<GrayAlpha8>(|i| (i * 37) as u8);
}

/// libtiff reads a grey + alpha document and a bilevel one page by page.
#[test]
fn gamut_pages_of_other_layouts_are_decoded_by_libtiff() {
    let (data, tiff) = two_pages::<GrayAlpha8>(|i| (i * 37) as u8);
    for (i, page) in data.iter().enumerate() {
        let dec = libtiff_oracle::decode_page(&tiff, i as u32).expect("libtiff decode page");
        assert_eq!(dec.samples_per_pixel, 2);
        assert_eq!(&dec.pixels, page, "grey + alpha page {i}");
    }
    let (data, tiff) = two_pages::<Bilevel>(|i| if i % 3 == 0 { 255 } else { 0 });
    for (i, page) in data.iter().enumerate() {
        let dec = libtiff_oracle::decode_page(&tiff, i as u32).expect("libtiff decode page");
        assert_eq!(&dec.pixels, page, "bilevel page {i}");
    }
}
