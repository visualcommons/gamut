//! Exif/XMP container-box tests:
//!
//! - the `Exif` box carries exactly the attached payload behind the standard 4-byte tiff-header
//!   offset, and the `xml ` box carries the XMP packet verbatim (pinned with a hand-rolled ISO
//!   BMFF box scanner — no decoder in the loop);
//! - a stream carrying metadata boxes still decodes bit-exactly in both gamut's pure-Rust decoder
//!   and the libjxl oracle;
//! - metadata combined with [`Container::Codestream`] is a typed error (boxes need the container),
//!   as is an empty payload.
//!
//! Uses both codec halves; compiled only when both are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{DecodedSamples, decode, gen_u8};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ErrorKind, ImageBuf, ImageRef, Rgb8};
use gamut_jxl::{Container, JxlDecoder, JxlEncoder};

/// A tiny TIFF-structured EXIF payload (little-endian byte-order mark; contents are opaque to the
/// box layer, which is what these tests pin).
const EXIF: &[u8] = b"II\x2A\x00\x08\x00\x00\x00test-exif-payload";
/// A minimal XMP packet.
const XMP: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">gamut-jxl test</x:xmpmeta>"#;

/// Encodes the deterministic 12x9 RGB8 pattern losslessly into the container with the given
/// metadata configuration applied.
fn encode_with(configure: impl FnOnce(JxlEncoder) -> JxlEncoder) -> Vec<u8> {
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    configure(JxlEncoder::lossless().with_container(Container::IsoBmff))
        .encode_image(image, &mut out)
        .expect("encode failed");
    out
}

/// Walks the top-level ISO BMFF box sequence and returns the payload of the first box with the
/// given type. Understands the 32-bit size form plus the `size == 0` (to end of file) convention;
/// the 64-bit extended form does not occur at these payload sizes.
fn find_box(data: &[u8], box_type: &[u8; 4]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let ty = &data[pos + 4..pos + 8];
        let end = if size == 0 { data.len() } else { pos + size };
        assert!(size == 0 || size >= 8, "malformed box size at offset {pos}");
        assert!(end <= data.len(), "box overruns the stream at offset {pos}");
        if ty == box_type {
            return Some(data[pos + 8..end].to_vec());
        }
        if size == 0 {
            break;
        }
        pos = end;
    }
    None
}

#[test]
fn exif_box_carries_the_payload_behind_the_tiff_offset() {
    let jxl = encode_with(|enc| enc.with_exif(EXIF));
    let payload = find_box(&jxl, b"Exif").expect("stream must contain an Exif box");
    // The Exif box format: 4-byte big-endian offset to the tiff header (0), then the raw EXIF.
    let mut expected = vec![0, 0, 0, 0];
    expected.extend_from_slice(EXIF);
    assert_eq!(payload, expected, "Exif box payload mismatch");
}

#[test]
fn xmp_box_carries_the_packet_verbatim() {
    let jxl = encode_with(|enc| enc.with_xmp(XMP));
    let payload = find_box(&jxl, b"xml ").expect("stream must contain an xml box");
    assert_eq!(payload, XMP.as_bytes(), "xml box payload mismatch");
}

#[test]
fn metadata_boxes_leave_the_pixels_bit_exact() {
    let jxl = encode_with(|enc| enc.with_exif(EXIF).with_xmp(XMP));
    // Both boxes present at once...
    assert!(find_box(&jxl, b"Exif").is_some());
    assert!(find_box(&jxl, b"xml ").is_some());

    // ...and the image itself is untouched, in both decoders.
    let samples = gen_u8(12, 9, 3);
    let image: ImageBuf<Rgb8> = JxlDecoder::new()
        .decode_image(&jxl)
        .expect("gamut decode of a metadata-carrying stream failed");
    assert_eq!(image.as_samples(), samples.as_slice(), "gamut");
    let oracle = decode(&jxl);
    assert_eq!(oracle.samples, DecodedSamples::U8(samples), "oracle");
}

#[test]
fn metadata_with_codestream_framing_is_rejected() {
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    // Default framing is Codestream; boxes cannot exist there.
    let err = JxlEncoder::lossless()
        .with_exif(EXIF)
        .encode_image(image, &mut out)
        .expect_err("metadata without the container must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
    assert_eq!(
        err.static_message(),
        Some("JXL: Exif/XMP metadata requires the ISO BMFF container")
    );
    assert!(out.is_empty(), "no output on the rejected path");
}

#[test]
fn empty_metadata_payload_is_rejected() {
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let mut out = Vec::new();
    let err = JxlEncoder::lossless()
        .with_container(Container::IsoBmff)
        .with_xmp("")
        .encode_image(image, &mut out)
        .expect_err("an empty metadata payload must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
    assert_eq!(err.static_message(), Some("JXL: empty metadata payload"));
}

#[test]
fn decoder_reads_the_exif_and_xmp_boxes_back() {
    // The read-back half of the box contract: what `with_exif` / `with_xmp` wrote comes back as
    // the TIFF stream (offset applied) and the packet, with no ICC for the default sRGB signal.
    let jxl = encode_with(|enc| enc.with_exif(EXIF).with_xmp(XMP));
    let meta = JxlDecoder::new().metadata(&jxl).expect("metadata");
    assert_eq!(meta.exif.as_deref(), Some(EXIF));
    assert_eq!(meta.xmp.as_deref(), Some(XMP.as_bytes()));
    assert_eq!(meta.icc, None);

    // A container with no boxes, and a bare codestream, both report nothing.
    let empty = encode_with(|enc| enc);
    assert_eq!(
        JxlDecoder::new().metadata(&empty).expect("metadata"),
        gamut_jxl::JxlMetadata::default()
    );
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let bare = JxlEncoder::lossless().encode_to_vec(image).unwrap();
    assert_eq!(
        JxlDecoder::new().metadata(&bare).expect("metadata"),
        gamut_jxl::JxlMetadata::default()
    );
}

#[test]
fn metadata_reports_the_codestream_icc_profile() {
    // The ICC field is the codestream's embedded profile (the libjxl-synthesized sRGB one, as the
    // colour tests use), byte-for-byte, alongside the boxes.
    let srgb = common::icc_profile(&encode_with(|enc| enc)).expect("oracle synthesizes sRGB");
    let jxl = encode_with(|enc| {
        enc.with_color(gamut_jxl::ColorSpec::Icc(srgb.clone()))
            .with_xmp(XMP)
    });
    let meta = JxlDecoder::new().metadata(&jxl).expect("metadata");
    assert_eq!(meta.icc.as_deref(), Some(srgb.as_slice()));
    assert_eq!(meta.xmp.as_deref(), Some(XMP.as_bytes()));
    assert_eq!(meta.exif, None);
}
