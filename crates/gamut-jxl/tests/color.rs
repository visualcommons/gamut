//! Colour-signalling differential tests against the reference libjxl oracle:
//!
//! - each [`ColorSpec`] variant produces exactly the structured colour encoding it promises
//!   (verified field-by-field via `JxlDecoderGetColorAsEncodedProfile`);
//! - lossless streams stay **bit-exact** through gamut's pure-Rust decoder for every built-in
//!   colour spec (colour signalling never touches the samples);
//! - an [`ColorSpec::Icc`] profile is carried verbatim (byte-exact ICC round-trip through the
//!   oracle) and replaces the structured encoding;
//! - structural ICC mismatches (wrong colour family, truncated profile) are typed errors.
//!
//! Uses both codec halves; compiled only when both are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{DecodedSamples, decode, encoded_color_profile, gen_u8, gen_u16, icc_profile};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, Gray16, ImageBuf, ImageRef, Rgb8, Rgb16,
};
use gamut_jxl::{ColorSpec, Distance, JxlDecoder, JxlEncoder};
use gamut_jxl_sys::types as ty;

/// Encodes a deterministic 24x17 RGB8 image losslessly with the given colour spec.
fn encode_rgb8(color: ColorSpec) -> Vec<u8> {
    let dims = Dimensions::new(24, 17).unwrap();
    let samples = gen_u8(24, 17, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    JxlEncoder::lossless()
        .with_color(color)
        .encode_image(image, &mut out)
        .expect("encode failed");
    out
}

/// Encodes a deterministic 24x17 RGB16 image losslessly with the given colour spec.
fn encode_rgb16(color: ColorSpec) -> (Vec<u16>, Vec<u8>) {
    let dims = Dimensions::new(24, 17).unwrap();
    let samples = gen_u16(24, 17, 3);
    let image = ImageRef::<Rgb16>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    JxlEncoder::lossless()
        .with_color(color)
        .encode_image(image, &mut out)
        .expect("encode failed");
    (samples, out)
}

/// Asserts the oracle-reported structured encoding matches the expected signal fields.
fn assert_signalled(
    jxl: &[u8],
    space: ty::JxlColorSpace,
    transfer: ty::JxlTransferFunction,
    primaries: ty::JxlPrimaries,
) {
    let enc = encoded_color_profile(jxl).expect("stream should carry a structured encoding");
    assert_eq!(enc.color_space, space, "colour space");
    assert_eq!(enc.transfer_function, transfer, "transfer function");
    assert_eq!(enc.white_point, ty::JxlWhitePoint::D65, "white point");
    if space != ty::JxlColorSpace::GRAY {
        assert_eq!(enc.primaries, primaries, "primaries");
    }
}

#[test]
fn srgb_default_signals_srgb() {
    let jxl = encode_rgb8(ColorSpec::Srgb);
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::RGB,
        ty::JxlTransferFunction::SRGB,
        ty::JxlPrimaries::SRGB,
    );
    // `with_color` untouched must signal the same thing as the explicit default.
    let dims = Dimensions::new(24, 17).unwrap();
    let samples = gen_u8(24, 17, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut plain = Vec::new();
    JxlEncoder::lossless()
        .encode_image(image, &mut plain)
        .unwrap();
    assert_eq!(
        plain, jxl,
        "default and explicit sRGB must encode identically"
    );
}

#[test]
fn linear_srgb_signals_linear_transfer() {
    let jxl = encode_rgb8(ColorSpec::LinearSrgb);
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::RGB,
        ty::JxlTransferFunction::LINEAR,
        ty::JxlPrimaries::SRGB,
    );
}

#[test]
fn pq_signals_bt2100_pq() {
    let (_, jxl) = encode_rgb16(ColorSpec::Pq);
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::RGB,
        ty::JxlTransferFunction::PQ,
        ty::JxlPrimaries::BT2100,
    );
}

#[test]
fn hlg_signals_bt2100_hlg() {
    let (_, jxl) = encode_rgb16(ColorSpec::Hlg);
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::RGB,
        ty::JxlTransferFunction::HLG,
        ty::JxlPrimaries::BT2100,
    );
}

#[test]
fn gray_pq_signals_gray_color_space() {
    let dims = Dimensions::new(9, 11).unwrap();
    let samples = gen_u16(9, 11, 1);
    let image = ImageRef::<Gray16>::new(&samples, dims).unwrap();
    let mut jxl = Vec::new();
    JxlEncoder::lossless()
        .with_color(ColorSpec::Pq)
        .encode_image(image, &mut jxl)
        .expect("encode failed");
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::GRAY,
        ty::JxlTransferFunction::PQ,
        ty::JxlPrimaries::BT2100,
    );
}

#[test]
fn lossless_is_bit_exact_for_every_built_in_color_spec() {
    // Colour signalling declares interpretation; it must never alter lossless samples, and jxl-rs
    // passes non-XYB streams through in the embedded profile without any CMS transform.
    for color in [
        ColorSpec::Srgb,
        ColorSpec::LinearSrgb,
        ColorSpec::Pq,
        ColorSpec::Hlg,
    ] {
        let (samples, jxl) = encode_rgb16(color.clone());

        let image: ImageBuf<Rgb16> = JxlDecoder::new()
            .decode_image(&jxl)
            .unwrap_or_else(|e| panic!("gamut decode failed for {color:?}: {e:?}"));
        assert_eq!(image.as_samples(), samples.as_slice(), "gamut ({color:?})");

        let oracle = decode(&jxl);
        assert_eq!(
            oracle.samples,
            DecodedSamples::U16(samples.clone()),
            "oracle ({color:?})"
        );
    }
}

/// A real sRGB ICC profile, synthesized by libjxl itself from a plain sRGB stream.
fn srgb_icc() -> Vec<u8> {
    icc_profile(&encode_rgb8(ColorSpec::Srgb)).expect("oracle should synthesize an sRGB profile")
}

#[test]
fn icc_profile_is_carried_verbatim() {
    let icc = srgb_icc();
    let jxl = encode_rgb8(ColorSpec::Icc(icc.clone()));

    // An attached ICC profile replaces the structured encoding...
    assert!(
        encoded_color_profile(&jxl).is_none(),
        "ICC streams must not report a structured encoding"
    );
    // ...and comes back byte-for-byte.
    assert_eq!(
        icc_profile(&jxl).expect("ICC profile must be readable"),
        icc,
        "attached ICC bytes must round-trip exactly"
    );

    // The samples themselves are untouched: lossless stays bit-exact in gamut's decoder.
    let samples = gen_u8(24, 17, 3);
    let image: ImageBuf<Rgb8> = JxlDecoder::new()
        .decode_image(&jxl)
        .expect("gamut decode of an ICC stream failed");
    assert_eq!(image.as_samples(), samples.as_slice());
}

#[test]
fn gray_icc_on_gray_image_roundtrips() {
    // Synthesize a genuine grayscale profile from a plain gray stream first.
    let dims = Dimensions::new(9, 11).unwrap();
    let samples = gen_u8(9, 11, 1);
    let image = ImageRef::<Gray8>::new(&samples, dims).unwrap();
    let mut plain = Vec::new();
    JxlEncoder::lossless()
        .encode_image(image, &mut plain)
        .unwrap();
    let gray_icc = icc_profile(&plain).expect("oracle should synthesize a gray profile");

    let image = ImageRef::<Gray8>::new(&samples, dims).unwrap();
    let mut jxl = Vec::new();
    JxlEncoder::lossless()
        .with_color(ColorSpec::Icc(gray_icc.clone()))
        .encode_image(image, &mut jxl)
        .expect("gray ICC encode failed");
    assert_eq!(icc_profile(&jxl).expect("ICC must be readable"), gray_icc);
}

#[test]
fn icc_color_family_mismatch_is_rejected() {
    let rgb_icc = srgb_icc();
    let dims = Dimensions::new(9, 11).unwrap();
    let samples = gen_u8(9, 11, 1);
    let image = ImageRef::<Gray8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    let err = JxlEncoder::lossless()
        .with_color(ColorSpec::Icc(rgb_icc))
        .encode_image(image, &mut out)
        .expect_err("an RGB profile on a grayscale image must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
    assert!(out.is_empty(), "no output on the rejected path");
}

#[test]
fn truncated_icc_is_rejected() {
    let dims = Dimensions::new(9, 11).unwrap();
    let samples = gen_u8(9, 11, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    let err = JxlEncoder::lossless()
        .with_color(ColorSpec::Icc(vec![0u8; 64]))
        .encode_image(image, &mut out)
        .expect_err("a truncated ICC profile must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
}

#[test]
fn decoder_surfaces_the_embedded_icc_profile() {
    let icc = srgb_icc();
    let jxl = encode_rgb8(ColorSpec::Icc(icc.clone()));
    let embedded = JxlDecoder::new()
        .embedded_icc_profile(&jxl)
        .expect("metadata parse failed")
        .expect("an ICC stream must surface its profile");
    assert_eq!(
        embedded, icc,
        "surfaced ICC bytes must match what was attached"
    );
}

#[test]
fn info_reports_whether_pixels_decode_into_the_embedded_profile() {
    // Lossless keeps the original colour space; lossy codes XYB, which decodes to sRGB rather
    // than into the embedded ICC profile.
    let icc = srgb_icc();
    let lossless = encode_rgb8(ColorSpec::Icc(icc.clone()));
    assert!(
        JxlDecoder::new()
            .info(&lossless)
            .unwrap()
            .uses_original_profile
    );

    let samples = gen_u8(24, 17, 3);
    let image = ImageRef::<Rgb8>::new(&samples, Dimensions::new(24, 17).unwrap()).unwrap();
    let mut lossy = Vec::new();
    JxlEncoder::lossy(Distance::new(1.0).unwrap())
        .with_color(ColorSpec::Icc(icc))
        .encode_image(image, &mut lossy)
        .expect("lossy ICC encode failed");
    assert!(
        !JxlDecoder::new()
            .info(&lossy)
            .unwrap()
            .uses_original_profile
    );
}

#[test]
fn decoder_reports_no_embedded_icc_for_structured_encodings() {
    // Structured encodings (sRGB, PQ, ...) carry no profile bytes: `None`, not a synthesized ICC.
    for color in [ColorSpec::Srgb, ColorSpec::Pq] {
        let jxl = encode_rgb8(color.clone());
        assert_eq!(
            JxlDecoder::new()
                .embedded_icc_profile(&jxl)
                .expect("metadata parse failed"),
            None,
            "{color:?} must not report an embedded ICC profile"
        );
    }
}

#[test]
fn lossy_pq_encodes_and_decodes() {
    // Lossy (XYB) with an HDR transfer signalled: libjxl converts through its built-in CMS on
    // encode, and jxl-rs renders back to the embedded PQ encoding on decode.
    let dims = Dimensions::new(24, 17).unwrap();
    let samples = gen_u16(24, 17, 3);
    let image = ImageRef::<Rgb16>::new(&samples, dims).unwrap();
    let mut jxl = Vec::new();
    JxlEncoder::lossy(Distance::new(1.0).unwrap())
        .with_color(ColorSpec::Pq)
        .encode_image(image, &mut jxl)
        .expect("lossy PQ encode failed");
    assert_signalled(
        &jxl,
        ty::JxlColorSpace::RGB,
        ty::JxlTransferFunction::PQ,
        ty::JxlPrimaries::BT2100,
    );

    let image: ImageBuf<Rgb16> = JxlDecoder::new()
        .decode_image(&jxl)
        .expect("gamut decode of a lossy PQ stream failed");
    assert_eq!(image.dimensions(), dims);
    // XYB round-trips are approximate; sanity-bound the reconstruction error rather than pinning
    // the exact tolerance of a perceptual pipeline in an HDR transfer domain.
    let mut sse = 0.0f64;
    for (&a, &b) in image.as_samples().iter().zip(&samples) {
        let d = f64::from(a) - f64::from(b);
        sse += d * d;
    }
    let mse = sse / samples.len() as f64;
    let psnr = 20.0 * 65535.0f64.log10() - 10.0 * mse.log10();
    assert!(psnr >= 30.0, "lossy PQ PSNR too low: {psnr:.1} dB");
}
