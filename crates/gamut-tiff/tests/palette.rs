//! Palette-colour images: tier-1 round-trips and libtiff cross-checks (P6).

use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, ImageBuf, ImageRef, Indexed8, Rgb8,
};
use gamut_tiff::{Compression, Palette, TiffDecoder, TiffEncoder};

const SIZES: &[(u32, u32)] = &[(1, 1), (5, 4), (17, 13), (64, 40)];

/// A 256-entry RGB palette with all three channels varying independently.
fn palette() -> Vec<u8> {
    let mut p = Vec::with_capacity(256 * 3);
    for i in 0..256u32 {
        p.push(i as u8);
        p.push((255 - i) as u8);
        p.push((i.wrapping_mul(7)) as u8);
    }
    p
}

fn indices(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(3).wrapping_add(y.wrapping_mul(5))) as u8);
        }
    }
    v
}

/// The RGB each index resolves to through the palette.
fn expected_rgb(idx: &[u8], pal: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(idx.len() * 3);
    for &i in idx {
        v.extend_from_slice(&pal[i as usize * 3..i as usize * 3 + 3]);
    }
    v
}

#[test]
fn palette_roundtrips_in_gamut() {
    let pal = palette();
    for &comp in &[Compression::None, Compression::PackBits] {
        for &(w, h) in SIZES {
            let idx = indices(w, h);
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_compression(comp)
                .encode_indexed8(
                    ImageRef::<Indexed8>::new(&idx, Dimensions::new(w, h).unwrap()).unwrap(),
                    &Palette::new(pal.as_chunks::<3>().0).unwrap(),
                    &mut tiff,
                )
                .expect("encode");
            let rgb: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(
                (rgb.dimensions().width(), rgb.dimensions().height()),
                (w, h)
            );
            assert_eq!(
                rgb.as_samples(),
                expected_rgb(&idx, &pal).as_slice(),
                "{comp:?} {w}x{h}"
            );
        }
    }
}

#[test]
fn gamut_palette_is_resolved_by_libtiff() {
    let pal = palette();
    for &(w, h) in SIZES {
        let idx = indices(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .encode_indexed8(
                ImageRef::<Indexed8>::new(&idx, Dimensions::new(w, h).unwrap()).unwrap(),
                &Palette::new(pal.as_chunks::<3>().0).unwrap(),
                &mut tiff,
            )
            .expect("encode");

        // libtiff resolves the ColorMap → RGB, validating the colour map against the reference.
        let (rw, rh, rgba) = libtiff_oracle::decode_rgba(&tiff).expect("libtiff rgba");
        assert_eq!((rw, rh), (w, h));
        let got_rgb: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        assert_eq!(got_rgb, expected_rgb(&idx, &pal), "rgb {w}x{h}");

        // The raw samples libtiff reads are the original palette indices.
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff samples");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 1));
        assert_eq!(dec.pixels, idx, "indices {w}x{h}");
    }
}

/// `DecodeImage<Indexed8>` returns the indices encoded, not the colour they select.
#[test]
fn palette_indices_roundtrip_in_gamut() {
    let pal = palette();
    for &(w, h) in SIZES {
        let idx = indices(w, h);
        let mut tiff = Vec::new();
        TiffEncoder::new()
            .encode_indexed8(
                ImageRef::<Indexed8>::new(&idx, Dimensions::new(w, h).unwrap()).unwrap(),
                &Palette::new(pal.as_chunks::<3>().0).unwrap(),
                &mut tiff,
            )
            .expect("encode");
        let got: ImageBuf<Indexed8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
        assert_eq!(got.as_samples(), idx.as_slice(), "{w}x{h}");
    }
}

/// `decode_page_indexed8` returns the indices with the colour table, padded to the 256 entries a
/// `ColorMap` always holds.
#[test]
fn decode_page_indexed8_returns_indices_and_colour_map() {
    let short = [[10, 20, 30], [40, 50, 60], [70, 80, 90]];
    let idx = [2u8, 0, 1, 1, 2, 0];
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .encode_indexed8(
            ImageRef::<Indexed8>::new(&idx, Dimensions::new(3, 2).unwrap()).unwrap(),
            &Palette::new(&short).unwrap(),
            &mut tiff,
        )
        .expect("encode");
    let (image, table) = TiffDecoder::new()
        .decode_page_indexed8(&tiff, 0)
        .expect("decode");
    assert_eq!(image.as_samples(), &idx);
    assert_eq!(
        (image.dimensions().width(), image.dimensions().height()),
        (3, 2)
    );
    let entries = table.rgb_entries();
    assert_eq!(entries.len(), 256);
    assert_eq!(&entries[..3], &short);
    assert_eq!(entries[255], [0, 0, 0]);
}

/// Only a palette page carries indices: a grey page is refused by both index entry points rather
/// than having its samples passed off as indices.
#[test]
fn indexed8_refuses_a_page_without_a_palette() {
    let tiff = TiffEncoder::new()
        .encode_to_vec(
            ImageRef::<Gray8>::new(&[1, 2, 3, 4], Dimensions::new(2, 2).unwrap()).unwrap(),
        )
        .expect("encode");
    let dec = TiffDecoder::new();
    let err = DecodeImage::<Indexed8>::decode_image(&dec, &tiff).expect_err("decoded");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    let err = dec.decode_page_indexed8(&tiff, 0).expect_err("decoded");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}
