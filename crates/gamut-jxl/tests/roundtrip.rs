//! End-to-end round-trip and conversion-contract tests for the pure-Rust decoder: gamut encodes a
//! stream with libjxl, gamut decodes it back with jxl-rs, and the result must be **bit-exact** to
//! the source (lossless) or obey the documented channel-conversion contracts.
//!
//! These exercise the encoder and decoder as a pair, so they need both codec halves; the module is
//! compiled only when both are available (i.e. off `wasm32`, with default features).
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

use gamut_core::convert::{AlphaPolicy, ConvertPolicy, LumaPolicy};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, Gray16, GrayAlpha8, GrayAlpha16,
    ImageBuf, ImageRef, Pixel, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_jxl::{BitDepth, Container, JxlDecoder, JxlEncoder};

/// The size grid: 1x1 up to a non-square textured image, including odd dimensions.
const SIZES: [(u32, u32); 5] = [(1, 1), (3, 7), (16, 16), (17, 13), (64, 100)];

/// A deterministic per-sample value in `0..=max`, non-flat so lossless is meaningfully exercised.
fn raw(x: u32, y: u32, c: u32, max: u32) -> u32 {
    let gradient = x.wrapping_mul(4).wrapping_add(y.wrapping_mul(3));
    let texture = ((x / 8) ^ (y / 8)).wrapping_mul(5);
    let channel = c.wrapping_mul(37);
    let base = gradient.wrapping_add(texture).wrapping_add(channel);
    let scale = if max > 0xFF { 251 } else { 1 };
    base.wrapping_mul(scale) & max
}

/// Generates interleaved 8-bit samples.
fn gen_u8(w: u32, h: u32, ch: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * ch);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch as u32 {
                v.push(raw(x, y, c, 0xFF) as u8);
            }
        }
    }
    v
}

/// Generates interleaved 16-bit samples.
fn gen_u16(w: u32, h: u32, ch: usize) -> Vec<u16> {
    let mut v = Vec::with_capacity(w as usize * h as usize * ch);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch as u32 {
                v.push(raw(x, y, c, 0xFFFF) as u16);
            }
        }
    }
    v
}

/// Emits a lossless round-trip test for one 8-bit layout, over the size grid and both containers.
macro_rules! roundtrip_u8 {
    ($name:ident, $pixel:ty) => {
        #[test]
        fn $name() {
            let ch = <$pixel as Pixel>::CHANNELS;
            for (w, h) in SIZES {
                for container in [Container::Codestream, Container::IsoBmff] {
                    let px = gen_u8(w, h, ch);
                    let dims = Dimensions::new(w, h).unwrap();
                    let img = ImageRef::<$pixel>::new(&px, dims).unwrap();
                    let bytes = JxlEncoder::lossless()
                        .with_container(container)
                        .encode_to_vec(img)
                        .unwrap();
                    let out: ImageBuf<$pixel> = JxlDecoder::new().decode_image(&bytes).unwrap();
                    assert_eq!(out.dimensions(), dims, "dims {w}x{h} {container:?}");
                    assert_eq!(
                        out.as_samples(),
                        px.as_slice(),
                        "lossless not bit-exact at {w}x{h} {container:?}"
                    );
                }
            }
        }
    };
}

/// Emits a lossless round-trip test for one 16-bit layout.
macro_rules! roundtrip_u16 {
    ($name:ident, $pixel:ty) => {
        #[test]
        fn $name() {
            let ch = <$pixel as Pixel>::CHANNELS;
            for (w, h) in SIZES {
                for container in [Container::Codestream, Container::IsoBmff] {
                    let px = gen_u16(w, h, ch);
                    let dims = Dimensions::new(w, h).unwrap();
                    let img = ImageRef::<$pixel>::new(&px, dims).unwrap();
                    let bytes = JxlEncoder::lossless()
                        .with_container(container)
                        .encode_to_vec(img)
                        .unwrap();
                    let out: ImageBuf<$pixel> = JxlDecoder::new().decode_image(&bytes).unwrap();
                    assert_eq!(out.dimensions(), dims, "dims {w}x{h} {container:?}");
                    assert_eq!(
                        out.as_samples(),
                        px.as_slice(),
                        "lossless not bit-exact at {w}x{h} {container:?}"
                    );
                }
            }
        }
    };
}

roundtrip_u8!(roundtrip_gray8, Gray8);
roundtrip_u8!(roundtrip_gray_alpha8, GrayAlpha8);
roundtrip_u8!(roundtrip_rgb8, Rgb8);
roundtrip_u8!(roundtrip_rgba8, Rgba8);
roundtrip_u16!(roundtrip_gray16, Gray16);
roundtrip_u16!(roundtrip_gray_alpha16, GrayAlpha16);
roundtrip_u16!(roundtrip_rgb16, Rgb16);
roundtrip_u16!(roundtrip_rgba16, Rgba16);

/// Encodes a lossless RGBA8 image and returns `(bytes, source pixels, dims)`.
fn encode_rgba8(w: u32, h: u32) -> (Vec<u8>, Vec<u8>, Dimensions) {
    let dims = Dimensions::new(w, h).unwrap();
    let px = gen_u8(w, h, Rgba8::CHANNELS);
    let img = ImageRef::<Rgba8>::new(&px, dims).unwrap();
    let bytes = JxlEncoder::lossless().encode_to_vec(img).unwrap();
    (bytes, px, dims)
}

#[test]
fn decode_image_into_reuses_allocation_when_dims_match() {
    let (bytes, px, dims) = encode_rgba8(17, 13);

    // Seed a destination of the matching geometry, remember its buffer identity, decode into it.
    let mut dst = ImageBuf::<Rgba8>::zeroed(dims).unwrap();
    let ptr_before = dst.as_samples().as_ptr();
    JxlDecoder::new()
        .decode_image_into(&bytes, &mut dst)
        .unwrap();
    let ptr_after = dst.as_samples().as_ptr();

    assert_eq!(dst.as_samples(), px.as_slice(), "decoded pixels correct");
    assert_eq!(
        ptr_before, ptr_after,
        "matching dimensions must reuse the destination's sample allocation"
    );
}

#[test]
fn decode_image_into_replaces_buffer_on_dimension_mismatch() {
    let (bytes, px, dims) = encode_rgba8(16, 16);

    // A destination of the wrong geometry must be replaced, and end up holding the decoded image.
    let mut dst = ImageBuf::<Rgba8>::zeroed(Dimensions::new(4, 4).unwrap()).unwrap();
    JxlDecoder::new()
        .decode_image_into(&bytes, &mut dst)
        .unwrap();
    assert_eq!(
        dst.dimensions(),
        dims,
        "destination resized to decoded dims"
    );
    assert_eq!(dst.as_samples(), px.as_slice(), "decoded pixels correct");
}

// ---- Conversion contracts: request a layout that differs from the stream's ----

#[test]
fn gray_stream_expands_to_rgb8_and_rgba8() {
    let (w, h) = (17, 13);
    let dims = Dimensions::new(w, h).unwrap();
    let gray = gen_u8(w, h, Gray8::CHANNELS);
    let img = ImageRef::<Gray8>::new(&gray, dims).unwrap();
    let bytes = JxlEncoder::lossless().encode_to_vec(img).unwrap();

    // Grayscale -> RGB8: each luminance replicated across R, G, B.
    let rgb: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&bytes).unwrap();
    assert_eq!(rgb.dimensions(), dims);
    for (i, &g) in gray.iter().enumerate() {
        assert_eq!(&rgb.as_samples()[i * 3..i * 3 + 3], &[g, g, g], "px {i}");
    }

    // Grayscale -> RGBA8: replicated colour plus a fully-opaque alpha.
    let rgba: ImageBuf<Rgba8> = JxlDecoder::new().decode_image(&bytes).unwrap();
    for (i, &g) in gray.iter().enumerate() {
        assert_eq!(
            &rgba.as_samples()[i * 4..i * 4 + 4],
            &[g, g, g, 0xFF],
            "px {i}"
        );
    }
}

#[test]
fn rgba_stream_decodes_as_rgb8_dropping_alpha() {
    let (bytes, px, dims) = encode_rgba8(16, 16);

    // Discarding a present alpha channel is a loss, so the default decoder refuses it rather than
    // deciding for the caller.
    let refused = DecodeImage::<Rgb8>::decode_image(&JxlDecoder::new(), &bytes)
        .expect_err("alpha drop must not happen silently");
    assert_eq!(refused.kind(), ErrorKind::Unsupported);

    let rgb: ImageBuf<Rgb8> = JxlDecoder::new()
        .with_convert_policy(ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop))
        .decode_image(&bytes)
        .unwrap();
    assert_eq!(rgb.dimensions(), dims);
    // Every RGB triple matches the source's colour channels; the source alpha is discarded.
    for i in 0..(16 * 16) {
        assert_eq!(
            &rgb.as_samples()[i * 3..i * 3 + 3],
            &px[i * 4..i * 4 + 3],
            "px {i}"
        );
    }
}

#[test]
fn opaque_rgba_stream_roundtrips_as_rgba8_bit_exact() {
    // An RGBA source whose alpha is entirely opaque must survive a lossless round-trip unchanged.
    let (w, h) = (16, 16);
    let dims = Dimensions::new(w, h).unwrap();
    let mut px = gen_u8(w, h, Rgba8::CHANNELS);
    for i in 0..(w * h) as usize {
        px[i * 4 + 3] = 0xFF; // force opaque alpha
    }
    let img = ImageRef::<Rgba8>::new(&px, dims).unwrap();
    let bytes = JxlEncoder::lossless().encode_to_vec(img).unwrap();
    let out: ImageBuf<Rgba8> = JxlDecoder::new().decode_image(&bytes).unwrap();
    assert_eq!(out.as_samples(), px.as_slice(), "opaque RGBA bit-exact");
}

#[test]
fn color_stream_requested_as_gray8_is_unsupported() {
    let (bytes, _px, _dims) = encode_rgba8(16, 16);
    let err = <JxlDecoder as DecodeImage<Gray8>>::decode_image(&JxlDecoder::new(), &bytes)
        .expect_err("a colour image cannot be decoded as grayscale");
    assert!(
        err.kind() == gamut_core::ErrorKind::Unsupported,
        "expected Unsupported, got {err:?}"
    );

    // ...and the same holds for the RGB (no-alpha) colour source.
    let rgb_px = gen_u8(16, 16, Rgb8::CHANNELS);
    let dims = Dimensions::new(16, 16).unwrap();
    let rgb_bytes = JxlEncoder::lossless()
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb_px, dims).unwrap())
        .unwrap();
    let err = <JxlDecoder as DecodeImage<GrayAlpha8>>::decode_image(&JxlDecoder::new(), &rgb_bytes)
        .expect_err("colour as gray+alpha is also refused");
    assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
}

#[test]
fn sixteen_bit_stream_requested_as_rgb8_downscales() {
    // Decoding a 16-bit lossless stream into an 8-bit request exercises jxl-rs's bit-depth
    // reduction. jxl-rs does *not* truncate the high byte (`>> 8`); it renormalises through f32 and
    // rounds — the exact observed rule is `round(v / 65535 * 255)` (nearest, ties away from zero),
    // the natural full-range rescale. We assert that exact rule, and separately that it stays within
    // ±1 of a plain high-byte truncation (a coarse sanity bound).
    let (w, h) = (16, 16);
    let dims = Dimensions::new(w, h).unwrap();
    let px16 = gen_u16(w, h, Rgb16::CHANNELS);
    let img = ImageRef::<Rgb16>::new(&px16, dims).unwrap();
    let bytes = JxlEncoder::lossless().encode_to_vec(img).unwrap();

    let out8: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&bytes).unwrap();
    assert_eq!(out8.dimensions(), dims);
    for (i, &s16) in px16.iter().enumerate() {
        let got = out8.as_samples()[i];
        let exact = (f32::from(s16) / 65535.0 * 255.0).round() as u8;
        assert_eq!(
            got, exact,
            "16->8 rule mismatch at sample {i}: 0x{s16:04X} -> {got} (want {exact})"
        );
        // Sanity: never more than one code off a naive high-byte truncation.
        let truncated = (s16 >> 8) as u8;
        assert!(
            got.abs_diff(truncated) <= 1,
            "16->8 at sample {i}: {got} vs truncation {truncated} differ by >1"
        );
    }
}

/// A 12-bit image carried in u16 buffers: `with_bit_depth(12)` declares the coded depth on
/// encode, and `with_codestream_bit_depth(true)` reads it back at that depth — the pair must be
/// bit-exact on the 12-bit code values. This is the framing DNG tiles (e.g. Apple ProRAW's
/// 10-bit LinearRaw) rely on.
#[test]
fn coded_bit_depth_roundtrips_sub16_code_values_exactly() {
    let (w, h) = (17, 13);
    let dims = Dimensions::new(w, h).unwrap();
    for ch in [1usize, 3] {
        let px: Vec<u16> = gen_u16(w, h, ch).iter().map(|&s| s & 0x0FFF).collect();
        let encode = |img: &[u16]| -> Vec<u8> {
            let enc = JxlEncoder::lossless().with_bit_depth(BitDepth::Twelve);
            if ch == 1 {
                enc.encode_to_vec(ImageRef::<Gray16>::new(img, dims).unwrap())
                    .unwrap()
            } else {
                enc.encode_to_vec(ImageRef::<Rgb16>::new(img, dims).unwrap())
                    .unwrap()
            }
        };
        let bytes = encode(&px);

        // The stream declares 12-bit integer samples.
        let info = JxlDecoder::new().info(&bytes).expect("info");
        assert_eq!(info.bits_per_sample, 12);
        assert!(!info.is_float);
        assert_eq!(info.color_channels, ch as u8);
        assert!(!info.has_alpha);
        assert_eq!(info.dimensions, dims);

        // Codestream-depth decode returns the exact 12-bit code values...
        let dec = JxlDecoder::new().with_codestream_bit_depth(true);
        let got: Vec<u16> = if ch == 1 {
            let out: ImageBuf<Gray16> = dec.decode_image(&bytes).unwrap();
            out.as_samples().to_vec()
        } else {
            let out: ImageBuf<Rgb16> = dec.decode_image(&bytes).unwrap();
            out.as_samples().to_vec()
        };
        assert_eq!(got, px, "{ch}-channel 12-bit code values must round-trip");

        // ...while the default full-range decode rescales them (the two policies must differ for
        // any non-zero sample, pinning that the knob actually changes the output range).
        let full: ImageBuf<Gray16>;
        let full_samples: &[u16] = if ch == 1 {
            full = JxlDecoder::new().decode_image(&bytes).unwrap();
            full.as_samples()
        } else {
            &[]
        };
        if ch == 1 {
            for (i, (&coded, &fullr)) in px.iter().zip(full_samples).enumerate() {
                // jxl-rs renormalises through f32, so allow one code of rounding play — the
                // point here is only that full-range output is *rescaled*, not the coded values.
                let expect = (f64::from(coded) / 4095.0 * 65535.0).round() as i64;
                assert!(
                    (i64::from(fullr) - expect).abs() <= 1,
                    "full-range decode at {i}: {fullr} (want ~{expect})"
                );
            }
        }
    }
}

/// The coded-depth override is validated: wider than the sample width is a typed error.
#[test]
fn with_bit_depth_rejects_incoherent_depths() {
    let dims = Dimensions::new(4, 4).unwrap();
    let px8 = gen_u8(4, 4, 1);
    let img = ImageRef::<Gray8>::new(&px8, dims).unwrap();
    for bits in [BitDepth::Ten, BitDepth::Twelve, BitDepth::Sixteen] {
        // Each is wider than the 8-bit sample width.
        let err = JxlEncoder::lossless()
            .with_bit_depth(bits)
            .encode_to_vec(img)
            .expect_err("incoherent coded depth must be rejected");
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput, "{bits:?}");
    }
    // An 8-bit override on an 8-bit layout is coherent (identity).
    assert!(
        JxlEncoder::lossless()
            .with_bit_depth(BitDepth::Eight)
            .encode_to_vec(img)
            .is_ok()
    );
}

/// `info` reports float sample types (which integer raw consumers must reject up front).
#[test]
fn info_reports_stream_properties_on_alpha_stream() {
    let dims = Dimensions::new(5, 3).unwrap();
    let px = gen_u8(5, 3, Rgba8::CHANNELS);
    let bytes = JxlEncoder::lossless()
        .encode_to_vec(ImageRef::<Rgba8>::new(&px, dims).unwrap())
        .unwrap();
    let info = JxlDecoder::new().info(&bytes).expect("info");
    assert_eq!(info.dimensions, dims);
    assert_eq!(info.bits_per_sample, 8);
    assert!(info.has_alpha);
    assert!(!info.is_float);
    assert!(!info.animated);
    // Junk input errors instead of panicking.
    assert!(JxlDecoder::new().info(&[0xFF, 0x0A]).is_err());
    assert!(JxlDecoder::new().info(&[]).is_err());
}

/// The conversion policy is readable back, and it reaches the decode.
///
/// `decode_samples` consults the policy *before* decoding, because the answer decides which layout
/// jxl-rs is asked for. A colour stream therefore cannot be presented as grayscale under the
/// default, and can once a `LumaPolicy` names the weights — and the two standards must disagree,
/// which pins that the policy's value is used rather than merely its presence.
#[test]
fn convert_policy_round_trips_and_reaches_the_decode() {
    let policy = ConvertPolicy::lossless().with_luma(LumaPolicy::Bt601);
    assert_eq!(
        JxlDecoder::new().convert_policy(),
        ConvertPolicy::lossless()
    );
    assert_eq!(
        JxlDecoder::new()
            .with_convert_policy(policy)
            .convert_policy(),
        policy
    );

    // Saturated red, so the chosen weights are plainly visible in the luma.
    let (w, h) = (16, 16);
    let dims = Dimensions::new(w, h).unwrap();
    let rgb: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 0]).collect();
    let bytes = JxlEncoder::lossless()
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims).unwrap())
        .expect("encode");

    let refused = DecodeImage::<Gray8>::decode_image(&JxlDecoder::new(), &bytes)
        .expect_err("colour as grayscale must not be guessed");
    assert_eq!(refused.kind(), ErrorKind::Unsupported);
    // The refusal must come from this crate, before the entropy decode -- not from gamut-core
    // afterwards. Both would report Unsupported, so the origin is what distinguishes "declined the
    // request" from "decoded the whole image and then declined".
    assert_eq!(refused.origin(), Some("gamut-jxl"));

    let bt601: ImageBuf<Gray8> = JxlDecoder::new()
        .with_convert_policy(ConvertPolicy::lossless().with_luma(LumaPolicy::Bt601))
        .decode_image(&bytes)
        .expect("bt601 decode");
    let bt709: ImageBuf<Gray8> = JxlDecoder::new()
        .with_convert_policy(ConvertPolicy::lossless().with_luma(LumaPolicy::Bt709))
        .decode_image(&bytes)
        .expect("bt709 decode");
    assert_eq!(bt601.as_samples()[0], 76); // round(0.299 * 255)
    assert_eq!(bt709.as_samples()[0], 54); // round(0.2126 * 255)

    // A grayscale stream still reaches a grayscale target with no policy at all: the guard must
    // narrow to "colour source", not to "grayscale target".
    let gray: Vec<u8> = (0..w * h).map(|i| (i % 256) as u8).collect();
    let gray_bytes = JxlEncoder::lossless()
        .encode_to_vec(ImageRef::<Gray8>::new(&gray, dims).unwrap())
        .expect("encode grey");
    let back: ImageBuf<Gray8> = JxlDecoder::new()
        .decode_image(&gray_bytes)
        .expect("grey decode needs no policy");
    assert_eq!(back.as_samples(), gray.as_slice());
}
