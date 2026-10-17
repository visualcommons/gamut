//! End-to-end tests for `gamut convert` to AVIF (issue #663): the input is decoded in its own
//! layout, so alpha, grey and 16-bit precision reach the encoder, and its ICC profile and Exif are
//! carried unless `--strip-metadata` is given. The `--avif-*` coding flags whose effect is visible
//! in the container (issue #664: matrix, depth, chroma) are read back here too; the rest are
//! pinned on the encoder the flags build, in `convert.rs`'s unit tests.
//!
//! These drive the built `gamut` binary for the reason `convert_metadata.rs` gives: `gamut-cli` is
//! outside the mutation globs and the coverage regex, so only an end-to-end run pins the wiring.
//! The encoder-side claims (how alpha, depth and metadata are coded) are pinned in `gamut-avif`;
//! each test here reads the output back with `gamut-avif`'s own container parser.

use std::process::Command;

use gamut::avif::{Av1Config, AvifContainer};
use gamut::core::{
    Dimensions, EncodeImage, Gray8, GrayAlpha8, ImageRef, Pixel, Rgb8, Rgb16, Rgba8,
};
use gamut::isobmff::ColourInformation;
use gamut::png::PngEncoder;

/// A 2×2 image.
const DIMS: Dimensions = Dimensions {
    width: 2,
    height: 2,
};

/// A bare TIFF stream holding one 0th-IFD tag, as a PNG `eXIf` carries it and as `gamut-exif`
/// writes it — so the command's parse-and-rewrite (which changes none of these tags) reproduces it
/// byte for byte.
fn exif() -> Vec<u8> {
    use gamut::exif::{ByteOrder, Exif, ExifTag, ExifWriter, Value};

    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
    ExifWriter::new().marker(false).write(&exif).unwrap()
}

/// An RGB profile, which the command carries verbatim onto RGB samples.
fn icc() -> Vec<u8> {
    use gamut::icc::{BuiltinProfile, IccProfile};

    IccProfile::builtin(BuiltinProfile::DisplayP3)
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// Encodes `samples` as a 2×2 PNG of pixel layout `P` through `encoder`.
fn png<P: Pixel>(encoder: PngEncoder, samples: &[P::Sample]) -> Vec<u8>
where
    PngEncoder: EncodeImage<P>,
{
    encoder
        .encode_to_vec(ImageRef::<P>::new(samples, DIMS).unwrap())
        .unwrap()
}

/// Converts `input` to AVIF with `extra` flags and returns the output file. Both temp files are
/// removed before the assertion runs.
fn convert(name: &str, input: &[u8], extra: &[&str]) -> Vec<u8> {
    let dir = std::env::temp_dir();
    let stem = format!("gamut-convert-avif-{}-{name}", std::process::id());
    let in_path = dir.join(format!("{stem}-in.png"));
    let out_path = dir.join(format!("{stem}-out.avif"));
    std::fs::write(&in_path, input).unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("convert")
        .arg(&in_path)
        .arg(&out_path)
        .args(extra)
        .output()
        .expect("run gamut convert");
    let encoded = std::fs::read(&out_path).ok();
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);

    assert!(
        run.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    encoded.expect("output written")
}

/// The pre-#663 path decoded every input to RGB, so the alpha channel never reached the encoder.
#[test]
fn rgba_png_gives_an_avif_with_an_alpha_item() {
    let rgba = [
        10, 20, 30, 0, 40, 50, 60, 85, 70, 80, 90, 170, 100, 110, 120, 255,
    ];
    let avif = convert("rgba", &png::<Rgba8>(PngEncoder::new(), &rgba), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    let image = container.image();
    assert!(
        image
            .alpha_auxiliary_of(image.primary_item().id())
            .is_some()
    );
}

/// The narrowest layout is chosen: an opaque PNG does not gain an alpha item it never had.
#[test]
fn opaque_rgb_png_gives_an_avif_without_an_alpha_item() {
    let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    let avif = convert("rgb", &png::<Rgb8>(PngEncoder::new(), &rgb), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    let image = container.image();
    assert!(
        image
            .alpha_auxiliary_of(image.primary_item().id())
            .is_none()
    );
}

/// The pre-#663 path narrowed 16-bit samples to 8; they now code at the encoder's default 12.
#[test]
fn sixteen_bit_png_codes_at_twelve_bits() {
    let rgb16 = [
        0, 0x1234, 0xFFFF, 0x8000, 1, 2, 3, 4, 5, 0xABCD, 0x0F0F, 0xF0F0,
    ];
    let avif = convert("rgb16", &png::<Rgb16>(PngEncoder::new(), &rgb16), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    let config = container
        .image()
        .primary_item()
        .av1_config()
        .expect("av1C")
        .unwrap();
    assert!(config.high_bitdepth && config.twelve_bit);
}

/// A greyscale PNG codes as one monochrome plane rather than three replicated ones.
#[test]
fn grey_png_codes_monochrome() {
    let grey = [0, 85, 170, 255];
    let avif = convert("grey", &png::<Gray8>(PngEncoder::new(), &grey), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    let config = container
        .image()
        .primary_item()
        .av1_config()
        .expect("av1C")
        .unwrap();
    assert!(config.monochrome);
}

/// A PNG carrying iCCP and eXIf, the two payloads the AVIF path carries.
fn tagged_png() -> Vec<u8> {
    let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    png::<Rgb8>(
        PngEncoder::new()
            .with_icc_profile("test", &icc())
            .with_exif(&exif()),
        &rgb,
    )
}

/// The input's ICC profile lands byte-equal in the output's `colr prof`.
#[test]
fn png_icc_profile_is_carried_into_colr_prof() {
    let avif = convert("icc", &tagged_png(), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    assert_eq!(
        container.image().primary_item().icc_profile(),
        Some(icc().as_slice())
    );
}

/// The input's Exif lands as the output's Exif item, behind HEIF's 4-byte tiff-header offset.
#[test]
fn png_exif_is_carried_into_the_exif_item() {
    let avif = convert("exif", &tagged_png(), &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    let item = container.image().exif().expect("Exif item");
    let payload = &item.as_isobmff_item().payload;
    assert_eq!(&payload[4..], exif().as_slice());
}

/// The input's XMP packet lands byte-equal as the output's XMP (`mime`) item.
#[test]
fn png_xmp_is_carried_into_the_xmp_item() {
    const XMP: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>";
    let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    let avif = convert(
        "xmp",
        &png::<Rgb8>(PngEncoder::new().with_xmp(XMP), &rgb),
        &[],
    );
    let container = AvifContainer::parse(&avif).unwrap();
    let item = container.image().xmp().expect("XMP item");
    assert_eq!(item.as_isobmff_item().payload, XMP.as_bytes());
}

/// A grey profile's bytes.
fn grey_icc() -> Vec<u8> {
    gamut::icc::IccProfile::gray_with_gamma(2.2)
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// A grey PNG codes one monochrome plane, which a `GRAY` profile describes (ICC.1:2022 §7.2.6),
/// so the profile is carried.
#[test]
fn a_grey_profile_is_carried_onto_monochrome_samples() {
    let grey = [0, 85, 170, 255];
    let input = png::<Gray8>(
        PngEncoder::new().with_icc_profile("grey", &grey_icc()),
        &grey,
    );
    let avif = convert("grey-icc", &input, &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    assert_eq!(
        container.image().primary_item().icc_profile(),
        Some(grey_icc().as_slice())
    );
}

/// Grey with alpha has no monochrome layout here and codes as RGBA, which a `GRAY` profile
/// cannot describe, so the profile is left behind.
#[test]
fn a_grey_profile_is_not_carried_onto_grey_with_alpha() {
    let grey_alpha = [0, 0, 85, 85, 170, 170, 255, 255];
    let input = png::<GrayAlpha8>(
        PngEncoder::new().with_icc_profile("grey", &grey_icc()),
        &grey_alpha,
    );
    let avif = convert("grey-alpha-icc", &input, &[]);
    let container = AvifContainer::parse(&avif).unwrap();
    assert_eq!(container.image().primary_item().icc_profile(), None);
}

/// An opaque 8-bit RGB PNG, the input the `--avif-*` coding-flag tests encode.
fn rgb_png() -> Vec<u8> {
    let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    png::<Rgb8>(PngEncoder::new(), &rgb)
}

/// The primary item's `av1C` of an AVIF file.
fn av1_config(avif: &[u8]) -> Av1Config {
    AvifContainer::parse(avif)
        .unwrap()
        .image()
        .primary_item()
        .av1_config()
        .expect("av1C")
        .unwrap()
}

/// `--avif-matrix` reaches the file: the `colr nclx` box names the chosen matrix (BT.601 is CICP
/// code point 6; the lossy default would be BT.709's 1).
#[test]
fn avif_matrix_is_written_into_colr() {
    let avif = convert(
        "matrix",
        &rgb_png(),
        &["--avif-lossy", "--avif-matrix", "bt601"],
    );
    let container = AvifContainer::parse(&avif).unwrap();
    let Some(ColourInformation::Nclx(nclx)) = container.image().primary_item().colour() else {
        panic!("no colr nclx");
    };
    assert_eq!(nclx.matrix_coefficients, 6);
}

/// `--avif-depth` reaches the file: an 8-bit input codes at 10 bits.
#[test]
fn avif_depth_codes_an_eight_bit_input_at_ten_bits() {
    let config = av1_config(&convert("depth", &rgb_png(), &["--avif-depth", "10"]));
    assert!(config.high_bitdepth && !config.twelve_bit);
}

/// `--avif-chroma` reaches the file: lossy 4:4:4 instead of the 4:2:0 default.
#[test]
fn avif_chroma_444_codes_full_resolution_chroma() {
    let config = av1_config(&convert(
        "chroma",
        &rgb_png(),
        &["--avif-lossy", "--avif-chroma", "444"],
    ));
    assert_eq!(
        (config.chroma_subsampling_x, config.chroma_subsampling_y),
        (0, 0)
    );
}

/// An out-of-range speed is refused by the argument parser before anything is encoded — cavif's
/// parser accepts 11–100 and then panics.
#[test]
fn out_of_range_avif_speed_is_a_usage_error() {
    let run = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .args(["convert", "in.png", "out.avif", "--avif-speed", "11"])
        .output()
        .expect("run gamut convert");
    // clap's usage-error exit status, not the conversion's failure status 1.
    assert_eq!(run.status.code(), Some(2));
}

/// `--strip-metadata` leaves both payloads out.
#[test]
fn strip_metadata_drops_icc_and_exif() {
    let avif = convert("strip", &tagged_png(), &["--strip-metadata"]);
    let container = AvifContainer::parse(&avif).unwrap();
    let image = container.image();
    assert_eq!(image.primary_item().icc_profile(), None);
    assert!(image.exif().is_none());
}
