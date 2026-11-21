//! Alpha-carrying images — RGBA (4 samples) and grey + alpha (2 samples) — and how each
//! `ExtraSamples` code (§18) is presented: tier-1 + libtiff cross-checks (P13).

use gamut_core::convert::{AlphaPolicy, ConvertPolicy, premultiply_alpha};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, GrayAlpha8, GrayAlpha16, ImageBuf, ImageRef,
    Pixel, Rgb8, Rgba8, Rgba16,
};
use gamut_tiff::{Compression, ExtraSample, TiffDecoder, TiffEncoder};
use libtiff_oracle::{AlphaLayout, encode_with_alpha, native_u16_bytes};

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
                .with_convert_policy(ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop))
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

/// `samples` with its colour premultiplied by its alpha — the associated form §18 code 1 stores.
fn premultiplied<P: Pixel>(samples: Vec<P::Sample>, w: u32, h: u32) -> Vec<P::Sample> {
    let mut image = ImageBuf::<P>::new(samples, Dimensions::new(w, h).unwrap()).unwrap();
    premultiply_alpha(&mut image);
    image.into_samples()
}

/// 16-bit samples whose low byte varies apart from the high one, `spp` to a pixel.
fn pattern16(w: u32, h: u32, spp: usize) -> Vec<u16> {
    (0..(w * h) as usize * spp)
        .map(|i| {
            let i = i as u32;
            (i.wrapping_mul(40_503) ^ (i >> 3).wrapping_mul(977)) as u16
        })
        .collect()
}

/// Each `ExtraSamples` code on a libtiff-written RGBA page — and the tag left out — decodes to the
/// unassociated pixels whose premultiplied form is exactly what libtiff's RGBA reader
/// (`TIFFReadRGBAImage`, which presents associated alpha) returns. libtiff reads `Unspecified` and
/// an absent tag on a four-sample RGB page as associated, so all but code 2 are un-premultiplied.
#[test]
fn libtiff_rgba_under_each_extra_samples_code_matches_libtiffs_rgba_read() {
    let (w, h) = (17, 13);
    let straight = rgba_pattern(w, h);
    let associated = premultiplied::<Rgba8>(straight.clone(), w, h);
    for (code, stored, declared) in [
        (Some(0), &associated, Some(ExtraSample::Unspecified)),
        (Some(1), &associated, Some(ExtraSample::AssociatedAlpha)),
        (Some(2), &straight, Some(ExtraSample::UnassociatedAlpha)),
        (None, &associated, None),
    ] {
        let tiff = encode_with_alpha(
            stored,
            w,
            h,
            AlphaLayout::Rgba,
            8,
            code,
            libtiff_oracle::Compression::Lzw,
        )
        .expect("libtiff encode");
        let dec = TiffDecoder::new();
        assert_eq!(dec.info(&tiff).unwrap().extra_sample, declared, "{code:?}");
        let got: ImageBuf<Rgba8> = dec.decode_image(&tiff).expect("gamut decode");
        let (_, _, libtiff) = libtiff_oracle::decode_rgba(&tiff).expect("libtiff RGBA read");
        assert_eq!(
            premultiplied::<Rgba8>(got.into_samples(), w, h),
            libtiff,
            "{code:?}"
        );
    }
}

/// Rewrites the first IFD's `ExtraSamples` (338) value — a single `SHORT`, so stored inline in its
/// entry — to `code`, in a classic TIFF of either byte order.
fn patch_extra_samples(tiff: &mut [u8], code: u16) {
    let le = &tiff[..2] == b"II";
    let u16_at = |b: &[u8], at: usize| {
        let pair = [b[at], b[at + 1]];
        if le {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let quad = [tiff[4], tiff[5], tiff[6], tiff[7]];
    let ifd = if le {
        u32::from_le_bytes(quad)
    } else {
        u32::from_be_bytes(quad)
    } as usize;
    let entry = (0..usize::from(u16_at(tiff, ifd)))
        .map(|i| ifd + 2 + 12 * i)
        .find(|&at| u16_at(tiff, at) == 338)
        .expect("ExtraSamples entry");
    assert_eq!(u16_at(tiff, entry + 2), 3, "ExtraSamples is a SHORT");
    let bytes = if le {
        code.to_le_bytes()
    } else {
        code.to_be_bytes()
    };
    tiff[entry + 8..entry + 10].copy_from_slice(&bytes);
}

/// Corel Draw writes unassociated alpha as `ExtraSamples = 999`, a code §18 does not assign;
/// libtiff patches it to `2` on read (`setExtraSamples`, `tif_dir.c`), and so does gamut. libtiff
/// itself cannot write `999` (it patches the value before writing too), so the fixture is its
/// code-2 page with the tag's value rewritten — and libtiff's RGBA read of that very file is the
/// oracle the straight decode is checked against.
#[test]
fn corel_draw_extra_samples_999_reads_as_unassociated_alpha_as_libtiff_reads_it() {
    let (w, h) = (17, 13);
    let straight = rgba_pattern(w, h);
    let mut tiff = encode_with_alpha(
        &straight,
        w,
        h,
        AlphaLayout::Rgba,
        8,
        Some(2),
        libtiff_oracle::Compression::Lzw,
    )
    .expect("libtiff encode");
    patch_extra_samples(&mut tiff, 999);
    let dec = TiffDecoder::new();
    assert_eq!(
        dec.info(&tiff).unwrap().extra_sample,
        Some(ExtraSample::UnassociatedAlpha)
    );
    let got: ImageBuf<Rgba8> = dec.decode_image(&tiff).expect("gamut decode");
    assert_eq!(got.as_samples(), straight.as_slice());
    let (_, _, libtiff) = libtiff_oracle::decode_rgba(&tiff).expect("libtiff RGBA read");
    assert_eq!(premultiplied::<Rgba8>(straight, w, h), libtiff);
}

/// The 16-bit RGBA path un-premultiplies associated alpha and passes unassociated alpha through.
/// libtiff's RGBA reader narrows to 8 bits, so the check is the law instead: re-premultiplying the
/// decode reproduces the well-formed associated page exactly.
#[test]
fn libtiff_rgba16_associated_alpha_is_unpremultiplied() {
    let (w, h) = (9, 7);
    let straight = pattern16(w, h, 4);
    let associated = premultiplied::<Rgba16>(straight.clone(), w, h);
    for (code, stored) in [(1, &associated), (2, &straight)] {
        let tiff = encode_with_alpha(
            &native_u16_bytes(stored),
            w,
            h,
            AlphaLayout::Rgba,
            16,
            Some(code),
            libtiff_oracle::Compression::None,
        )
        .expect("libtiff encode");
        let got: ImageBuf<Rgba16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
        if code == 2 {
            assert_eq!(got.as_samples(), straight.as_slice());
        } else {
            assert_ne!(got.as_samples(), associated.as_slice(), "un-premultiplied");
            assert_eq!(
                premultiplied::<Rgba16>(got.into_samples(), w, h),
                associated
            );
        }
    }
}

/// A libtiff-written grey + alpha page decodes at both depths: unassociated alpha passes
/// through, and associated alpha is un-premultiplied (re-premultiplying gives the stored page).
#[test]
fn libtiff_grey_alpha_is_decoded_by_gamut() {
    let (w, h) = (11, 6);
    let straight8: Vec<u8> = rgba_pattern(w, h)[..(w * h * 2) as usize].to_vec();
    let assoc8 = premultiplied::<GrayAlpha8>(straight8.clone(), w, h);
    let straight16 = pattern16(w, h, 2);
    let assoc16 = premultiplied::<GrayAlpha16>(straight16.clone(), w, h);
    for (code, declared) in [
        (1, ExtraSample::AssociatedAlpha),
        (2, ExtraSample::UnassociatedAlpha),
    ] {
        let associated = code == 1;
        let stored8 = if associated { &assoc8 } else { &straight8 };
        let tiff = encode_with_alpha(
            stored8,
            w,
            h,
            AlphaLayout::GrayAlpha,
            8,
            Some(code),
            libtiff_oracle::Compression::PackBits,
        )
        .expect("libtiff encode");
        assert_eq!(
            TiffDecoder::new().info(&tiff).unwrap().extra_sample,
            Some(declared)
        );
        let got: ImageBuf<GrayAlpha8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
        if associated {
            assert_ne!(got.as_samples(), assoc8.as_slice(), "un-premultiplied");
            assert_eq!(
                premultiplied::<GrayAlpha8>(got.into_samples(), w, h),
                assoc8
            );
        } else {
            assert_eq!(got.as_samples(), straight8.as_slice());
        }

        let stored16 = if associated { &assoc16 } else { &straight16 };
        let tiff = encode_with_alpha(
            &native_u16_bytes(stored16),
            w,
            h,
            AlphaLayout::GrayAlpha,
            16,
            Some(code),
            libtiff_oracle::Compression::None,
        )
        .expect("libtiff encode");
        let got: ImageBuf<GrayAlpha16> = TiffDecoder::new().decode_image(&tiff).expect("decode");
        if associated {
            assert_ne!(got.as_samples(), assoc16.as_slice(), "un-premultiplied");
            assert_eq!(
                premultiplied::<GrayAlpha16>(got.into_samples(), w, h),
                assoc16
            );
        } else {
            assert_eq!(got.as_samples(), straight16.as_slice());
        }
    }
}

/// A two-sample grey page whose second sample is not declared alpha — `Unspecified`, or no
/// `ExtraSamples` at all — has no alpha reading in libtiff either, and is refused rather than
/// guessed at; its declaration is still described.
#[test]
fn grey_with_an_extra_sample_not_declared_alpha_is_refused() {
    for (code, declared) in [(Some(0), Some(ExtraSample::Unspecified)), (None, None)] {
        let tiff = encode_with_alpha(
            &[10, 200, 30, 40],
            2,
            1,
            AlphaLayout::GrayAlpha,
            8,
            code,
            libtiff_oracle::Compression::None,
        )
        .expect("libtiff encode");
        let dec = TiffDecoder::new();
        assert_eq!(dec.info(&tiff).unwrap().extra_sample, declared, "{code:?}");
        let err = DecodeImage::<GrayAlpha8>::decode_image(&dec, &tiff).expect_err("decoded");
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{code:?}");
    }
}

/// gamut's grey + alpha encode declares `BlackIsZero` with unassociated alpha, and libtiff reads
/// back the stored samples unchanged at both depths — strips and Deflate-predicted tiles alike.
///
/// The width is a whole number of tiles: libtiff's scanline reader refuses tiles, so the 8-bit
/// tiled case reads through `TIFFReadRGBAImage`, whose grey + alpha path (`putagreytile`) does
/// not scale its source skew by the sample count and so misreads a cropped right-edge tile.
#[test]
fn gamut_grey_alpha_is_read_by_libtiff() {
    let (w, h) = (32, 7);
    let px8: Vec<u8> = rgba_pattern(w, h)[..(w * h * 2) as usize].to_vec();
    let px16 = pattern16(w, h, 2);
    let dims = Dimensions::new(w, h).unwrap();
    for encoder in [
        TiffEncoder::new().with_compression(Compression::Lzw),
        TiffEncoder::new()
            .with_compression(Compression::Deflate)
            .with_predictor(gamut_tiff::Predictor::HorizontalDifferencing)
            .with_tiling(16, 16),
    ] {
        let tiff = encoder
            .encode_to_vec(ImageRef::<GrayAlpha8>::new(&px8, dims).unwrap())
            .expect("encode");
        let info = TiffDecoder::new().info(&tiff).unwrap();
        assert_eq!(
            (info.samples_per_pixel, info.photometric, info.extra_sample),
            (
                2,
                gamut_tiff::PhotometricInterpretation::BlackIsZero,
                Some(ExtraSample::UnassociatedAlpha)
            )
        );
        let read = if info.tiled {
            libtiff_oracle::decode_rgba(&tiff)
                .expect("libtiff RGBA read")
                .2
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[3]])
                .collect()
        } else {
            libtiff_oracle::decode_tiff(&tiff)
                .expect("libtiff read")
                .pixels
        };
        assert_eq!(read, px8, "8-bit, tiled: {}", info.tiled);

        let tiff = encoder
            .encode_to_vec(ImageRef::<GrayAlpha16>::new(&px16, dims).unwrap())
            .expect("encode");
        let oracle = libtiff_oracle::decode_tiff16(&tiff).expect("libtiff read");
        assert_eq!(oracle.samples_per_pixel, 2);
        assert_eq!(oracle.samples, px16, "16-bit, tiled: {}", info.tiled);
    }
}

/// Grey + alpha round-trips through gamut at both depths, and in both byte orders for 16 bits.
#[test]
fn grey_alpha_roundtrips_in_gamut() {
    let (w, h) = (13, 5);
    let px8: Vec<u8> = rgba_pattern(w, h)[..(w * h * 2) as usize].to_vec();
    let px16 = pattern16(w, h, 2);
    let dims = Dimensions::new(w, h).unwrap();
    for order in [
        gamut_tiff::ByteOrder::LittleEndian,
        gamut_tiff::ByteOrder::BigEndian,
    ] {
        let encoder = TiffEncoder::new()
            .with_byte_order(order)
            .with_compression(Compression::PackBits);
        let tiff = encoder
            .encode_to_vec(ImageRef::<GrayAlpha8>::new(&px8, dims).unwrap())
            .unwrap();
        let got: ImageBuf<GrayAlpha8> = TiffDecoder::new().decode_image(&tiff).unwrap();
        assert_eq!(got.as_samples(), px8.as_slice(), "{order:?}");
        let tiff = encoder
            .encode_to_vec(ImageRef::<GrayAlpha16>::new(&px16, dims).unwrap())
            .unwrap();
        let got: ImageBuf<GrayAlpha16> = TiffDecoder::new().decode_image(&tiff).unwrap();
        assert_eq!(got.as_samples(), px16.as_slice(), "{order:?}");
    }
}
