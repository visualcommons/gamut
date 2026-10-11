//! The `metadata` feature through the container: the facade's keystone equality
//! (extract → embed → extract over EXIF / XMP / ICC) extended through a real `.jxl` stream, the
//! routing of each `EncodedMetadata` carrier to its setter, and the typed refusals for carriers the
//! container cannot write.
//!
//! Needs both codec halves plus the facade; compiled only when all are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    feature = "metadata",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::gen_u8;
use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8};
use gamut_jxl::{Container, EncodedMetadata, JxlDecoder, JxlEncoder, Metadata, MetadataBlock};
use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
use gamut_metadata::xmp::{WellKnownNs, XmpMeta};

/// Encodes the deterministic 12×9 RGB8 pattern losslessly with `encoder`.
fn encode(encoder: JxlEncoder) -> Vec<u8> {
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    encoder.encode_to_vec(image).expect("encode failed")
}

/// A container-framed lossless encoder.
fn container_encoder() -> JxlEncoder {
    JxlEncoder::lossless().with_container(Container::IsoBmff)
}

/// A typed model with EXIF, XMP and the libjxl-synthesized sRGB ICC profile, normalised through
/// one embed → extract pass so it is an *extracted* model (a hand-built model differs from its
/// parsed form in fields the serializer stamps).
fn typed() -> Metadata {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    let srgb = common::icc_profile(&encode(container_encoder())).expect("oracle synthesizes sRGB");
    let encoded = Metadata::from_carriers(Some(exif), Some(xmp), None)
        .encode()
        .unwrap();
    Metadata::from_blocks(&[
        MetadataBlock::Exif(encoded.exif.as_deref().unwrap()),
        MetadataBlock::Xmp(encoded.xmp.as_deref().unwrap()),
        MetadataBlock::Icc(&srgb),
    ])
    .unwrap()
}

#[test]
fn typed_metadata_round_trips_through_the_container() {
    let typed = typed();
    let jxl = encode(container_encoder().with_metadata(&typed).unwrap());
    let read = JxlDecoder::new().metadata(&jxl).unwrap();
    assert!(read.exif.is_some() && read.xmp.is_some() && read.icc.is_some());
    assert_eq!(read.blocks().len(), 3);
    assert_eq!(read.metadata().unwrap(), typed);
}

#[test]
fn a_manifest_store_is_never_copied_forward() {
    let mut typed = typed();
    typed.c2pa = Some(b"\0\0\0\x14jumbc2pa".to_vec());
    let jxl = encode(container_encoder().with_metadata(&typed).unwrap());
    let read = JxlDecoder::new()
        .metadata(&jxl)
        .unwrap()
        .metadata()
        .unwrap();
    assert_eq!(read.c2pa, None);
    typed.c2pa = None;
    assert_eq!(read, typed);
}

#[test]
fn encoded_blocks_route_to_the_setters_with_the_exif_signature_stripped() {
    let encoded = typed().encode().unwrap();
    let jxl = encode(container_encoder().with_encoded_metadata(&encoded).unwrap());
    let read = JxlDecoder::new().metadata(&jxl).unwrap();
    // `EncodedMetadata::exif` carries `Exif\0\0`; the `Exif` box carries the TIFF stream.
    assert_eq!(
        read.exif.as_deref(),
        encoded
            .exif
            .as_deref()
            .and_then(|e| e.strip_prefix(b"Exif\0\0"))
    );
    assert_eq!(read.xmp, encoded.xmp);
    assert_eq!(read.icc, encoded.icc);

    // An absent field leaves an earlier setting untouched.
    let mut only_xmp = EncodedMetadata::default();
    only_xmp.xmp = encoded.xmp.clone();
    let jxl = encode(
        container_encoder()
            .with_exif(b"II\x2A\x00\x08\x00\x00\x00\x00\x00")
            .with_encoded_metadata(&only_xmp)
            .unwrap(),
    );
    let read = JxlDecoder::new().metadata(&jxl).unwrap();
    assert_eq!(
        read.exif.as_deref(),
        Some(&b"II\x2A\x00\x08\x00\x00\x00\x00\x00"[..])
    );
    assert_eq!(read.xmp, encoded.xmp);
}

#[test]
fn unwritable_carriers_are_typed_errors() {
    let mut iim = EncodedMetadata::default();
    iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
    let err = container_encoder().with_encoded_metadata(&iim).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        err.static_message(),
        Some("JXL: IPTC-IIM has no container carrier")
    );

    let mut c2pa = EncodedMetadata::default();
    c2pa.c2pa = Some(vec![0u8; 4]);
    let err = container_encoder()
        .with_encoded_metadata(&c2pa)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        err.static_message(),
        Some("JXL: C2PA manifest store (jumb box) embedding is not supported")
    );

    let mut binary_xmp = EncodedMetadata::default();
    binary_xmp.xmp = Some(vec![0xFF, 0xFE, 0x00]);
    let err = container_encoder()
        .with_encoded_metadata(&binary_xmp)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.static_message(), Some("JXL: XMP packet is not UTF-8"));
}

#[test]
fn typed_metadata_still_needs_the_container_framing() {
    // The raw setters' rule holds unchanged: boxes need the ISO BMFF container.
    let dims = Dimensions::new(12, 9).unwrap();
    let samples = gen_u8(12, 9, 3);
    let image = ImageRef::<Rgb8>::new(&samples, dims).unwrap();
    let err = JxlEncoder::lossless()
        .with_metadata(&typed())
        .unwrap()
        .encode_to_vec(image)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("JXL: Exif/XMP metadata requires the ISO BMFF container")
    );
}
