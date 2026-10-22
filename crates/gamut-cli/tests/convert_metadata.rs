//! End-to-end tests for what `gamut convert` does with the input's metadata: carried by default
//! into every output that can hold it (issues #483 and #570), with the Exif claims a conversion
//! can make false rewritten, and dropped under `--strip-metadata`.
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) rather than calling the command
//! function, because `crates/gamut-cli` is outside both the mutation globs and the coverage
//! regex — behaviour pinned only by a unit test here is pinned nowhere the gates can see. The
//! encoder- and reader-side claims are pinned in the format crates; what this file adds is that
//! the CLI maps one format's metadata onto another's at all, which is exactly the gap the issues
//! reported.

use std::path::PathBuf;
use std::process::Command;

use gamut::core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageRef, Rgb8, Rgba8};
use gamut::exif::{ByteOrder, Exif, ExifTag, ExifWriter, Value};
use gamut::icc::{BuiltinProfile, IccProfile};
use gamut::ifd::tags::INTEROPERABILITY_IFD;
use gamut::jpeg::JpegEncoder;
use gamut::jxl::{Container, Distance, JxlDecoder, JxlEncoder, Orientation};
use gamut::png::{PngEncoder, PngMetadata, SrgbIntent};
use gamut::tiff::TiffDecoder;
use gamut::webp::WebpEncoder;

/// A bare TIFF stream, as `gamut-exif` writes it, holding `Make` plus whatever `tags` sets.
fn exif_with(tags: &[(ExifTag, Value)]) -> Vec<u8> {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
    for (tag, value) in tags {
        exif.set_tag(*tag, value.clone());
    }
    ExifWriter::new().marker(false).write(&exif).unwrap()
}

/// The Exif stream `exif`, parsed back.
fn parse_exif(exif: Option<&[u8]>) -> Exif {
    Exif::parse(exif.expect("an Exif payload")).expect("the Exif parses")
}

/// An RGB profile's bytes.
fn rgb_icc() -> Vec<u8> {
    IccProfile::builtin(BuiltinProfile::DisplayP3)
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// A grey profile's bytes.
fn grey_icc() -> Vec<u8> {
    IccProfile::gray_with_gamma(2.2)
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// A 2×2 PNG carrying an EXIF block, a text annotation, a rendering intent and a C2PA manifest
/// store — the last being the one payload a re-encode may not carry.
fn png_with_metadata() -> Vec<u8> {
    let rgba = vec![255u8; 4 * 4];
    let dims = Dimensions {
        width: 2,
        height: 2,
    };
    let image = ImageRef::<Rgba8>::new(&rgba, dims).unwrap();
    PngEncoder::new()
        .with_exif(&exif_with(&[]))
        .with_text("Author", "nobody")
        .with_srgb(SrgbIntent::Perceptual)
        .with_c2pa(b"\0\0\0\x10jumbc2pa")
        .encode_to_vec(image)
        .unwrap()
}

/// An 8×8 colour JPEG carrying `exif`, an XMP packet and an RGB ICC profile.
fn jpeg_with(exif: &[u8]) -> Vec<u8> {
    jpeg_sized(exif, 8, 8)
}

/// [`jpeg_with`] at `width`×`height`.
fn jpeg_sized(exif: &[u8], width: u32, height: u32) -> Vec<u8> {
    let rgb: Vec<u8> = (0..width * height * 3).map(|i| (i * 3) as u8).collect();
    let mut jpeg = Vec::new();
    JpegEncoder::new()
        .with_exif(exif)
        .with_xmp(XMP)
        .with_icc_profile(&rgb_icc())
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(width, height).unwrap()).unwrap(),
            &mut jpeg,
        )
        .unwrap();
    jpeg
}

/// The XMP packet the fixtures carry.
const XMP: &[u8] = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>";

/// Writes `png` to a temp file, converts it to PNG with `extra` flags, and returns the output's
/// metadata together with what the command said on stderr.
fn convert(name: &str, png: &[u8], extra: &[&str]) -> (PngMetadata, String) {
    let (encoded, stderr) = convert_bytes(name, png, extra);
    (gamut::png::metadata(&encoded).expect("read back"), stderr)
}

/// [`convert`], returning the output file itself rather than its metadata.
fn convert_bytes(name: &str, png: &[u8], extra: &[&str]) -> (Vec<u8>, String) {
    convert_to(name, png, "png", extra)
}

/// Converts `input` (whatever its format: the command sniffs it) to the format the extension
/// `to` names, with `extra` flags, and returns the output file and what the command said on
/// stderr. Both temp files are removed before the assertion runs.
fn convert_to(name: &str, input_bytes: &[u8], to: &str, extra: &[&str]) -> (Vec<u8>, String) {
    let dir = std::env::temp_dir();
    let input = dir.join(format!(
        "gamut-convert-{}-{name}-in.bin",
        std::process::id()
    ));
    let output: PathBuf = dir.join(format!(
        "gamut-convert-{}-{name}-out.{to}",
        std::process::id()
    ));
    std::fs::write(&input, input_bytes).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("convert")
        .arg(&input)
        .arg(&output)
        .args(extra)
        .output()
        .expect("run gamut convert");
    let encoded = std::fs::read(&output).ok();
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);

    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    (
        encoded.expect("output written"),
        String::from_utf8_lossy(&status.stderr).into_owned(),
    )
}

/// The issue's headline: `gamut convert` used to decode to raw RGBA and encode with a bare
/// builder, so every EXIF, ICC, XMP and text chunk was lost with no warning.
#[test]
fn png_to_png_carries_the_input_metadata_by_default() {
    let (meta, _) = convert("default", &png_with_metadata(), &[]);

    assert_eq!(meta.exif, Some(exif_with(&[])));
    assert_eq!(meta.srgb, Some(SrgbIntent::Perceptual));
    let texts: Vec<(&str, &str)> = meta
        .texts
        .iter()
        .map(|t| (t.keyword.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(texts, [("Author", "nobody")]);
}

/// The opt-out: a stripped file is smaller, which is why the flag exists, but it has to be asked
/// for — the default may not silently discard colour information.
#[test]
fn strip_metadata_drops_it_all() {
    let (meta, _) = convert("stripped", &png_with_metadata(), &["--strip-metadata"]);

    assert_eq!(meta, PngMetadata::default());
}

/// A payload the command could not carry is *said*, not swallowed. A C2PA manifest store is
/// signed over the bytes of the file it was made for (C2PA 2.4 §A.3.2), so a copy would be
/// invalid — but the caller asked for preservation and is entitled to know their provenance did
/// not survive. Warnings reach stderr at the default verbosity, so this needs no `-v`.
#[test]
fn a_payload_that_cannot_be_carried_is_reported_on_stderr() {
    let (meta, stderr) = convert("dropped", &png_with_metadata(), &[]);

    assert!(meta.c2pa.is_none(), "the store is not carried");
    assert!(
        stderr.contains("C2PA manifest store"),
        "stderr said nothing about the store: {stderr}"
    );
}

/// The other half of the report: a payload that *was* written, but not as §11.3.3 would have it,
/// is worded as carried rather than as lost. A trailing space in a keyword is one §11.3.3.1 does
/// not permit and this crate's reader accepts, so the annotation goes through verbatim.
#[test]
fn a_payload_carried_with_a_caveat_is_reported_as_carried() {
    let rgba = vec![255u8; 4 * 4];
    let image = ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap();
    let png = PngEncoder::new()
        .with_text("Author ", "nobody")
        .encode_to_vec(image)
        .unwrap();

    let (meta, stderr) = convert("caveat", &png, &[]);

    let keywords: Vec<&str> = meta.texts.iter().map(|t| t.keyword.as_str()).collect();
    assert_eq!(
        keywords,
        ["Author "],
        "the annotation is written as it arrived"
    );
    assert!(
        stderr.contains("input metadata carried with a caveat")
            && stderr.contains("leading, trailing or consecutive space"),
        "stderr did not report the carried annotation: {stderr}"
    );
    assert!(
        !stderr.contains("not carried"),
        "a written annotation was reported as lost: {stderr}"
    );
}

/// §11.3.2.3 pins an RGB profile to colour types 2, 3 and 6. `gamut convert` auto-reduces, and
/// the default path carries the input's profile, so grey content in an RGB file used to come out
/// as greyscale under the RGB profile it was converted with — a pairing libpng rejects, ignoring
/// the profile. The reduction now stays in the profile's family.
#[test]
fn grey_content_under_an_rgb_profile_is_not_reduced_to_greyscale() {
    let rgba: Vec<u8> = (0..64u8)
        .flat_map(|i| [i * 3; 3].into_iter().chain([255]))
        .collect();
    let image = ImageRef::<Rgba8>::new(&rgba, Dimensions::new(8, 8).unwrap()).unwrap();
    let mut icc = vec![0u8; 128];
    icc[16..20].copy_from_slice(b"RGB ");
    let png = PngEncoder::new()
        .with_icc_profile("rgb", &icc)
        .encode_to_vec(image)
        .unwrap();

    let (out, _) = convert_bytes("rgb-profile", &png, &[]);

    let color_type = out[25];
    assert!(
        matches!(color_type, 2 | 3 | 6),
        "colour type {color_type} under an RGB profile"
    );
    let meta = gamut::png::metadata(&out).expect("read back");
    assert_eq!(meta.icc_profile.map(|p| p.profile), Some(icc));
}

/// Issue #570's headline: a pair other than PNG→PNG carries the metadata too. A JPEG's Exif, XMP
/// and ICC profile reach a WebP's `EXIF`, `XMP ` and `ICCP` chunks.
#[test]
fn jpeg_to_webp_carries_exif_xmp_and_icc() {
    let (webp, _) = convert_to("jpeg-webp", &jpeg_with(&exif_with(&[])), "webp", &[]);

    let meta = gamut::webp::metadata(&webp).expect("read back");
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
    assert_eq!(meta.icc, Some(rgb_icc()));
    assert_eq!(
        parse_exif(meta.exif.as_deref()).make(),
        Some("gamut"),
        "the Exif is carried"
    );
}

/// `PixelXDimension` / `PixelYDimension` describe the image the file holds, so they become the
/// output's size rather than repeating the input's claim.
#[test]
fn pixel_dimensions_are_rewritten_to_the_output_size() {
    let exif = exif_with(&[
        (ExifTag::PixelXDimension, Value::Long(vec![4000])),
        (ExifTag::PixelYDimension, Value::Long(vec![3000])),
    ]);

    let (meta, _) = convert("pixel-dims", &jpeg_with(&exif), &[]);

    let exif = parse_exif(meta.exif.as_deref());
    assert_eq!(
        exif.get_tag(ExifTag::PixelXDimension),
        Some(&Value::Long(vec![8]))
    );
    assert_eq!(
        exif.get_tag(ExifTag::PixelYDimension),
        Some(&Value::Long(vec![8]))
    );
}

/// The JPEG XL decoder orients its pixels for display, so an Exif `Orientation` copied from the
/// input would rotate them a second time; it becomes 1.
#[test]
fn a_jxl_input_orientation_is_rewritten_to_identity() {
    let rgb: Vec<u8> = (0..4 * 2 * 3).map(|i| (i * 10) as u8).collect();
    let mut jxl = Vec::new();
    JxlEncoder::lossless()
        .with_container(Container::IsoBmff)
        .with_orientation(Orientation::Rotate90Cw)
        .with_exif(&exif_with(&[(ExifTag::Orientation, Value::Short(vec![6]))]))
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(4, 2).unwrap()).unwrap(),
            &mut jxl,
        )
        .unwrap();

    let (meta, _) = convert("jxl-orientation", &jxl, &[]);

    assert_eq!(parse_exif(meta.exif.as_deref()).orientation(), Some(1));
}

/// A JPEG's pixels are stored unrotated, so its `Orientation` still holds; a JPEG XL output
/// signals it in its own codestream header, which a bare codestream keeps even though it has no
/// box for the Exif — and the Exif left behind is reported, naming the flag that would carry it.
#[test]
fn exif_orientation_reaches_a_bare_jxl_codestream() {
    let exif = exif_with(&[(ExifTag::Orientation, Value::Short(vec![6]))]);

    let (jxl, stderr) = convert_to("jpeg-jxl", &jpeg_sized(&exif, 16, 8), "jxl", &[]);

    // The decoder applies the signalled orientation, and a quarter turn swaps the 16×8 coded
    // size into an 8×16 displayed one.
    let decoded: gamut::core::ImageBuf<Rgba8> = JxlDecoder::new().decode_image(&jxl).unwrap();
    assert_eq!(decoded.dimensions(), Dimensions::new(8, 16).unwrap());
    assert!(
        stderr.contains("--jxl-container"),
        "stderr did not name the flag: {stderr}"
    );
}

/// With `--jxl-container` the Exif and XMP reach the JPEG XL container's boxes.
#[test]
fn exif_and_xmp_reach_a_jxl_container() {
    let (jxl, _) = convert_to(
        "jpeg-jxlc",
        &jpeg_with(&exif_with(&[])),
        "jxl",
        &["--jxl-container"],
    );

    let meta = JxlDecoder::new().metadata(&jxl).unwrap();
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
    assert_eq!(parse_exif(meta.exif.as_deref()).make(), Some("gamut"));
}

/// A grey JPEG tagged with a grey profile, whose Exif claims sRGB.
fn grey_jpeg() -> Vec<u8> {
    let mut jpeg = Vec::new();
    JpegEncoder::new()
        .with_exif(&exif_with(&[(ExifTag::ColorSpace, Value::Short(vec![1]))]))
        .with_icc_profile(&grey_icc())
        .encode_image(
            ImageRef::<Gray8>::new(&[0x40; 64], Dimensions::new(8, 8).unwrap()).unwrap(),
            &mut jpeg,
        )
        .unwrap();
    jpeg
}

/// A WebP codes RGB, and a `GRAY` profile cannot describe RGB data (ICC.1:2022 §7.2.6), so the
/// profile is left behind — and said to be.
#[test]
fn a_grey_profile_is_not_carried_onto_colour_samples() {
    let (webp, stderr) = convert_to("grey-icc", &grey_jpeg(), "webp", &[]);

    assert_eq!(gamut::webp::metadata(&webp).unwrap().icc, None);
    assert!(
        stderr.contains("ICC profile") && stderr.contains("RGB samples"),
        "stderr did not report the profile: {stderr}"
    );
}

/// The pixels are still in the grey profile's space once that profile is left behind, so the
/// Exif's `ColorSpace` claim becomes Uncalibrated rather than staying sRGB.
#[test]
fn color_space_turns_uncalibrated_when_the_profile_is_left_behind() {
    let (webp, _) = convert_to("grey-colorspace", &grey_jpeg(), "webp", &[]);

    let exif = parse_exif(gamut::webp::metadata(&webp).unwrap().exif.as_deref());
    assert_eq!(
        exif.get_tag(ExifTag::ColorSpace),
        Some(&Value::Short(vec![0xFFFF]))
    );
}

/// A TIFF output carries the XMP (700) and ICC profile (34675) verbatim and the Exif as its
/// `ExifIFD` sub-IFD, rewritten like every other output's (issue #851).
#[test]
fn a_tiff_output_carries_xmp_icc_and_the_exif_sub_ifd() {
    let exif = exif_with(&[
        (ExifTag::ExposureTime, Value::Rational(vec![(1, 250)])),
        (ExifTag::PixelXDimension, Value::Long(vec![4000])),
    ]);

    let (tiff, _) = convert_to("tiff", &jpeg_with(&exif), "tiff", &[]);

    let meta = TiffDecoder::new().metadata(&tiff).expect("read back");
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
    assert_eq!(meta.icc, Some(rgb_icc()));
    let directory = meta.exif.expect("an ExifIFD");
    assert_eq!(
        directory.get(ExifTag::ExposureTime.tag_id()),
        Some(&Value::Rational(vec![(1, 250)]))
    );
    assert_eq!(
        directory.get(ExifTag::PixelXDimension.tag_id()),
        Some(&Value::Long(vec![8])),
        "rewritten to the output's size"
    );
}

/// A TIFF's 0th-IFD tags are the page's own, so the Exif's `Make` and `Orientation` arrive as tags
/// of the TIFF's IFD 0 (issue #876) — the Orientation telling a reader how to display the stored
/// pixels — while `YCbCrPositioning`, which IFD 0 holds as the TIFF encoder's own, is named as
/// left behind.
#[test]
fn a_tiff_output_carries_the_exif_0th_ifd_into_its_own_ifd_0() {
    let exif = exif_with(&[
        (ExifTag::Orientation, Value::Short(vec![6])),
        (ExifTag::YCbCrPositioning, Value::Short(vec![1])),
    ]);

    let (tiff, stderr) = convert_to("tiff-0th", &jpeg_with(&exif), "tiff", &[]);

    assert!(
        stderr.contains("Exif 0th-IFD field(s) YCbCrPositioning,"),
        "stderr did not name the 0th-IFD field left behind: {stderr}"
    );
    assert!(
        !stderr.contains("Make") && !stderr.contains("Orientation"),
        "stderr reported a carried field as left behind: {stderr}"
    );
    let meta = TiffDecoder::new().metadata(&tiff).expect("read back");
    let page = meta.ifd0.expect("the 0th-IFD fields");
    assert_eq!(
        page.get(ExifTag::Make.tag_id()),
        Some(&Value::Ascii("gamut".into()))
    );
    assert_eq!(
        page.get(ExifTag::Orientation.tag_id()),
        Some(&Value::Short(vec![6]))
    );
    assert_eq!(page.get(ExifTag::YCbCrPositioning.tag_id()), None);
    // The fixture's Exif holds nothing below its 0th IFD, so no `ExifIFD` is written, and the
    // other carriers still are.
    assert!(meta.exif.is_none());
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
}

/// The GPS sub-IFD hangs off a TIFF's own IFD 0 and arrives there (issue #876); the thumbnail
/// would be a page of its own, so a TIFF output names it as left behind — and the Exif sub-IFD,
/// with the Interoperability sub-IFD below it, is still written, as are XMP and ICC: the encoder
/// took the reduced model rather than refusing it.
#[test]
fn a_tiff_output_carries_the_gps_and_interop_ifds_and_reports_the_thumbnail() {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
    exif.set_tag(ExifTag::GpsLatitudeRef, Value::Ascii("N".into()));
    exif.set_tag(ExifTag::InteroperabilityIndex, Value::Ascii("R98".into()));
    let mut thumbnail = Vec::new();
    JpegEncoder::new()
        .encode_image(
            ImageRef::<Gray8>::new(&[128; 64], Dimensions::new(8, 8).unwrap()).unwrap(),
            &mut thumbnail,
        )
        .unwrap();
    exif.set_thumbnail(thumbnail);
    let exif = ExifWriter::new().marker(false).write(&exif).unwrap();

    let (tiff, stderr) = convert_to("tiff-gps-thumb", &jpeg_with(&exif), "tiff", &[]);

    assert!(
        !stderr.contains("GPS"),
        "stderr reported the carried GPS sub-IFD as left behind: {stderr}"
    );
    assert!(
        stderr.contains("the Exif thumbnail, which a TIFF would hold as a page of its own"),
        "stderr did not report the thumbnail: {stderr}"
    );
    let meta = TiffDecoder::new().metadata(&tiff).expect("read back");
    assert_eq!(
        meta.gps
            .expect("the GPS sub-IFD")
            .get(ExifTag::GpsLatitudeRef.tag_id()),
        Some(&Value::Ascii("N".into()))
    );
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
    assert_eq!(meta.icc, Some(rgb_icc()));
    let directory = meta.exif.expect("an ExifIFD");
    assert_eq!(
        directory.get(ExifTag::ExposureTime.tag_id()),
        Some(&Value::Rational(vec![(1, 250)]))
    );
    let interop = directory
        .sub_ifds()
        .iter()
        .find(|group| group.tag == INTEROPERABILITY_IFD)
        .and_then(|group| group.ifds.first())
        .expect("the Interoperability sub-IFD is kept");
    assert_eq!(
        interop.get(ExifTag::InteroperabilityIndex.tag_id()),
        Some(&Value::Ascii("R98".into()))
    );
}

/// A C2PA manifest store is never carried, between formats as within one.
#[test]
fn a_c2pa_store_is_not_carried_into_another_format() {
    let rgba = vec![255u8; 4 * 4];
    let mut webp = Vec::new();
    WebpEncoder::lossless()
        .with_c2pa(b"\0\0\0\x10jumbc2pa")
        .encode_image(
            ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap(),
            &mut webp,
        )
        .unwrap();

    let (meta, stderr) = convert("webp-c2pa", &webp, &[]);

    assert!(meta.c2pa.is_none(), "the store is not carried");
    assert!(
        stderr.contains("C2PA manifest store"),
        "stderr said nothing about the store: {stderr}"
    );
}

/// A PNG's text annotations and colour chunks have no `gamut-metadata` block, so another format
/// cannot hold them; they are named as left behind.
#[test]
fn png_only_chunks_are_reported_when_leaving_png() {
    let (_, stderr) = convert_to("png-webp", &png_with_metadata(), "webp", &[]);

    assert!(
        stderr.contains("tEXt/zTXt/iTXt") && stderr.contains("sRGB"),
        "stderr did not name the chunks: {stderr}"
    );
}

/// The opt-out covers every pair, not only PNG→PNG.
#[test]
fn strip_metadata_drops_it_between_formats() {
    let (webp, _) = convert_to(
        "jpeg-webp-stripped",
        &jpeg_with(&exif_with(&[])),
        "webp",
        &["--strip-metadata"],
    );

    let meta = gamut::webp::metadata(&webp).unwrap();
    assert_eq!((meta.exif, meta.xmp, meta.icc), (None, None, None));
}

/// A JPEG output attaches what it carries: the Exif in APP1, the XMP in its own APP1 and the ICC
/// profile in APP2. The input is a WebP, so nothing reaches the output by a JPEG→JPEG shortcut.
#[test]
fn a_jpeg_output_carries_exif_xmp_and_icc() {
    let rgba = vec![200u8; 8 * 8 * 4];
    let mut webp = Vec::new();
    WebpEncoder::lossless()
        .with_exif(&exif_with(&[]))
        .with_xmp(XMP)
        .with_icc_profile(&rgb_icc())
        .encode_image(
            ImageRef::<Rgba8>::new(&rgba, Dimensions::new(8, 8).unwrap()).unwrap(),
            &mut webp,
        )
        .unwrap();

    let (jpeg, _) = convert_to("webp-jpeg", &webp, "jpg", &[]);

    let meta = gamut::jpeg::metadata(&jpeg).expect("read back");
    assert_eq!(meta.xmp.as_deref(), Some(XMP));
    assert_eq!(meta.icc, Some(rgb_icc()));
    assert_eq!(parse_exif(meta.exif.as_deref()).make(), Some("gamut"));
}

/// The ICC profile lives in the JPEG XL codestream itself, so it is carried in the default bare
/// framing, where the Exif and XMP are not.
#[test]
fn the_icc_profile_reaches_a_jxl_codestream() {
    let (jxl, _) = convert_to("jpeg-jxl-icc", &jpeg_with(&exif_with(&[])), "jxl", &[]);

    assert_eq!(
        JxlDecoder::new().embedded_icc_profile(&jxl).unwrap(),
        Some(rgb_icc())
    );
}

/// A profile whose header parses but which libjxl refuses fails only the metadata, not the
/// conversion: the image is encoded without it and the refusal is reported, as on the JPEG path.
#[test]
fn a_profile_libjxl_refuses_does_not_fail_the_conversion() {
    let mut icc = vec![0u8; 132];
    icc[..4].copy_from_slice(&132u32.to_be_bytes());
    icc[12..16].copy_from_slice(b"mntr");
    icc[16..20].copy_from_slice(b"RGB ");
    icc[20..24].copy_from_slice(b"XYZ ");
    icc[36..40].copy_from_slice(b"acsp");
    let rgba = vec![255u8; 4 * 4];
    let png = PngEncoder::new()
        .with_icc_profile("header-only", &icc)
        .encode_to_vec(ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap())
        .unwrap();

    let (jxl, stderr) = convert_to("bad-icc-jxl", &png, "jxl", &[]);

    assert_eq!(JxlDecoder::new().embedded_icc_profile(&jxl).unwrap(), None);
    assert!(
        stderr.contains("the JPEG XL encoder refused it"),
        "stderr did not report the refusal: {stderr}"
    );
}

/// An XYB JPEG XL decodes into sRGB rather than into its embedded profile, so the Exif's
/// `ColorSpace` claim becomes 1 (sRGB) whatever the input said.
#[test]
fn color_space_becomes_srgb_after_an_xyb_jxl_input() {
    let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 5) as u8).collect();
    let mut jxl = Vec::new();
    JxlEncoder::lossy(Distance::new(1.0).unwrap())
        .with_container(Container::IsoBmff)
        .with_exif(&exif_with(&[(
            ExifTag::ColorSpace,
            Value::Short(vec![0xFFFF]),
        )]))
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap(),
            &mut jxl,
        )
        .unwrap();

    let (meta, _) = convert("xyb-colorspace", &jxl, &[]);

    assert_eq!(
        parse_exif(meta.exif.as_deref()).get_tag(ExifTag::ColorSpace),
        Some(&Value::Short(vec![1]))
    );
}

/// A PNG output from a non-PNG input attaches the ICC profile as `iCCP` and the XMP as `iTXt`
/// through the encoder's per-carrier setters, not a PNG input's whole chunk set.
#[test]
fn a_png_output_from_another_format_carries_icc_and_xmp() {
    let (meta, _) = convert("jpeg-png-icc-xmp", &jpeg_with(&exif_with(&[])), &[]);

    assert_eq!(
        meta.icc_profile.as_ref().map(|p| &p.profile),
        Some(&rgb_icc())
    );
    assert_eq!(meta.xmp(), Some(XMP));
}

/// A bare little-endian TIFF stream whose 1st IFD is an uncompressed one-strip thumbnail, its
/// three strip bytes `[1, 2, 3]` stored at offset 68 and addressed by a `StripOffsets` of
/// `strip_offset` — 68 to read them, past the end of the stream for a strip that does not read.
fn exif_with_strip_thumbnail(strip_offset: u32) -> Vec<u8> {
    let entry = |tag: u16, kind: u16, value: u32| {
        let mut e = Vec::new();
        e.extend_from_slice(&tag.to_le_bytes());
        e.extend_from_slice(&kind.to_le_bytes());
        e.extend_from_slice(&1u32.to_le_bytes());
        e.extend_from_slice(&value.to_le_bytes());
        e
    };
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    // Header (8) + 0th IFD of one entry (18) = 26; + 1st IFD of three entries (42) = 68.
    let mut tiff = b"II\x2A\x00\x08\x00\x00\x00".to_vec();
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend(entry(0x0112, SHORT, 1)); // Orientation
    tiff.extend_from_slice(&26u32.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend(entry(0x0103, SHORT, 1)); // Compression = uncompressed
    tiff.extend(entry(0x0111, LONG, strip_offset)); // StripOffsets
    tiff.extend(entry(0x0117, LONG, 3)); // StripByteCounts
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(&[1, 2, 3]);
    tiff
}

/// Converts a 2×2 PNG carrying [`exif_with_strip_thumbnail`]`(strip_offset)` to PNG, returning the
/// output's parsed Exif and what the command said on stderr.
fn convert_strip_thumbnail(name: &str, strip_offset: u32) -> (Exif, String) {
    let rgba = vec![255u8; 4 * 4];
    let png = PngEncoder::new()
        .with_exif(&exif_with_strip_thumbnail(strip_offset))
        .encode_to_vec(ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap())
        .unwrap();
    let (meta, stderr) = convert(name, &png, &[]);
    (parse_exif(meta.exif.as_deref()), stderr)
}

/// `gamut-exif` re-embeds an uncompressed thumbnail's strips, so a strip thumbnail is carried with
/// its bytes rather than dropped.
#[test]
fn a_strip_thumbnail_is_carried_with_its_bytes() {
    let (exif, stderr) = convert_strip_thumbnail("strip-thumbnail", 68);

    let thumb = exif.thumbnail().expect("the thumbnail is written");
    assert_eq!(thumb.strips(), Some([vec![1u8, 2, 3]].as_slice()));
    assert!(
        !stderr.contains("Exif thumbnail"),
        "stderr reported a carried thumbnail as dropped: {stderr}"
    );
}

/// A thumbnail whose strips do not read holds no image bytes: the reader has already removed its
/// `StripOffsets`, so it would be written as a thumbnail with no image, its `StripByteCounts`
/// measuring bytes that are not there; it is dropped instead, and said to be.
#[test]
fn an_unreadable_strip_thumbnail_is_dropped_rather_than_written_imageless() {
    let (exif, stderr) = convert_strip_thumbnail("unreadable-strip-thumbnail", 4096);

    assert_eq!(
        exif.orientation(),
        Some(1),
        "the rest of the Exif is carried"
    );
    assert!(exif.thumbnail().is_none(), "the thumbnail is not written");
    assert!(
        stderr.contains("Exif thumbnail"),
        "stderr did not report the thumbnail: {stderr}"
    );
}
