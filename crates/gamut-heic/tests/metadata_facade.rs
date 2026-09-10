//! The `metadata` feature over a HEIF fixture: `HeifImage::blocks` / `metadata` hand the Exif
//! item (offset applied), the XMP `mime` item and the `colr` ICC profile to the facade, and the
//! typed model equals the one the payloads were built from. Decode-only (the crate has no
//! encoder), so the fixture is authored through `gamut_isobmff::write`.
#![cfg(feature = "metadata")]

mod common;

use common::{clean_file, hvc1_item, iref, item};
use gamut_core::ErrorKind;
use gamut_heic::{HeifContainer, Metadata, MetadataBlock};
use gamut_isobmff::{ColourInformation, Item, Property, PropertyKind};
use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
use gamut_metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader};
use gamut_metadata::xmp::{WellKnownNs, XmpMeta};

/// The three carrier payloads as the leaf crates serialize them: an `Exif\0\0`-prefixed EXIF
/// blob, an XMP packet and an ICC profile.
fn payloads() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    let icc = IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: Vec::new(),
    };
    let encoded = Metadata::from_carriers(Some(exif), Some(xmp), Some(icc))
        .encode()
        .unwrap();
    (
        encoded.exif.unwrap(),
        encoded.xmp.unwrap(),
        encoded.icc.unwrap(),
    )
}

/// A HEIF whose primary (1) carries a `prof` ICC `colr`, described by an Exif item (2) with the
/// given `ExifDataBlock` payload and an XMP `mime` item (3).
fn fixture(exif_payload: Vec<u8>, xmp: Vec<u8>, icc: Vec<u8>) -> Vec<u8> {
    let mut primary = hvc1_item(1, vec![1, 2, 3, 4]);
    primary.properties.push(Property {
        essential: false,
        kind: PropertyKind::Colour(ColourInformation::UnrestrictedIcc(icc)),
    });
    let exif = Item {
        references: vec![iref(b"cdsc", &[1])],
        ..item(2, *b"Exif", exif_payload)
    };
    let xmp = Item {
        content_type: Some("application/rdf+xml".to_string()),
        references: vec![iref(b"cdsc", &[1])],
        ..item(3, *b"mime", xmp)
    };
    clean_file(1, vec![primary, exif, xmp])
}

/// `offset` as a big-endian `exif_tiff_header_offset` followed by `rest`.
fn exif_data_block(offset: u32, rest: &[u8]) -> Vec<u8> {
    let mut out = offset.to_be_bytes().to_vec();
    out.extend_from_slice(rest);
    out
}

#[test]
fn typed_metadata_is_extracted_from_the_items_and_the_colr() {
    let (exif, xmp, icc) = payloads();
    // The model the payloads came from, as the facade itself extracts it (the ICC header's
    // `size` is stamped by serialization, so the hand-built model is not the comparison point).
    let expected = Metadata::from_blocks(&[
        MetadataBlock::Exif(&exif),
        MetadataBlock::Xmp(&xmp),
        MetadataBlock::Icc(&icc),
    ])
    .unwrap();

    // Offset 0 over the bare TIFF stream (the usual authoring), and offset 6 keeping the
    // `Exif\0\0` signature in front of the TIFF header — both locate the same stream.
    let tiff = exif.strip_prefix(b"Exif\0\0").unwrap();
    for exif_payload in [exif_data_block(0, tiff), exif_data_block(6, &exif)] {
        let data = fixture(exif_payload, xmp.clone(), icc.clone());
        let container = HeifContainer::parse(&data).unwrap();
        let image = container.image();

        let blocks = image.blocks().unwrap();
        assert_eq!(
            blocks,
            vec![
                MetadataBlock::Exif(tiff),
                MetadataBlock::Xmp(&xmp),
                MetadataBlock::Icc(&icc),
            ]
        );
        assert_eq!(image.metadata().unwrap(), expected);
    }
}

#[test]
fn a_file_without_metadata_yields_an_empty_model() {
    let data = clean_file(1, vec![hvc1_item(1, vec![1, 2, 3, 4])]);
    let container = HeifContainer::parse(&data).unwrap();
    assert!(container.image().blocks().unwrap().is_empty());
    assert_eq!(container.image().metadata().unwrap(), Metadata::default());
}

#[test]
fn a_malformed_exif_item_is_invalid_input_from_both_accessors() {
    let (_, xmp, icc) = payloads();
    // Three bytes: shorter than the offset field itself.
    let data = fixture(vec![0, 0, 0], xmp, icc);
    let container = HeifContainer::parse(&data).unwrap();
    for err in [
        container.image().blocks().unwrap_err(),
        container.image().metadata().unwrap_err(),
    ] {
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("HEIF: Exif item payload is shorter than its tiff-header offset field")
        );
    }
}

#[test]
fn an_unparsable_payload_is_invalid_input_with_the_facade_detail() {
    let (exif, xmp, _) = payloads();
    let tiff = exif.strip_prefix(b"Exif\0\0").unwrap();
    // A `prof` colr whose bytes are not an ICC profile: located fine, refused by the facade.
    let data = fixture(
        exif_data_block(0, tiff),
        xmp,
        b"not an icc profile".to_vec(),
    );
    let container = HeifContainer::parse(&data).unwrap();
    assert_eq!(container.image().blocks().unwrap().len(), 3);
    let err = container.image().metadata().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIF: embedded metadata does not parse")
    );
    assert!(err.detail().is_some_and(|d| d.starts_with("ICC:")), "{err}");
}
