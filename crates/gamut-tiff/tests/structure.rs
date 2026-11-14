//! Structural round-trip and robustness tests for the TIFF container layer (P2).

use gamut_ifd::{TiffFile, read, write};
use gamut_tiff::{ByteOrder, Ifd, Value, Variant, tags};

#[test]
fn public_api_roundtrips_a_directory() {
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![320]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![200]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
    ifd.set(tags::X_RESOLUTION, Value::Rational(vec![(150, 1)]));
    ifd.set(305, Value::Ascii("gamut".to_owned())); // Software tag, out-of-line

    // Both byte orders and both container variants (classic TIFF and BigTIFF) round-trip.
    for variant in [Variant::Classic, Variant::Big] {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let file = TiffFile {
                order,
                variant,
                ifds: vec![ifd.clone()],
            };
            let bytes = write(&file).expect("write");
            let parsed = read(&bytes).expect("read back");
            assert_eq!(parsed, file);
            assert_eq!(parsed.variant, variant);
            assert_eq!(parsed.ifds[0].get_u32(tags::IMAGE_WIDTH), Some(320));
            assert_eq!(
                parsed.ifds[0].get_u32_vec(tags::BITS_PER_SAMPLE),
                Some(vec![8, 8, 8])
            );
        }
    }
}

/// A minimal little-endian TIFF with one `ImageWidth=4` SHORT entry; `next` is the caller's choice
/// and `type_code` lets a test inject an unknown field type.
fn minimal_le(type_code: u16, next: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"II");
    b.extend_from_slice(&42u16.to_le_bytes());
    b.extend_from_slice(&8u32.to_le_bytes()); // first IFD at offset 8
    b.extend_from_slice(&1u16.to_le_bytes()); // entry count
    b.extend_from_slice(&256u16.to_le_bytes()); // ImageWidth
    b.extend_from_slice(&type_code.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // count
    b.extend_from_slice(&4u32.to_le_bytes()); // inline value
    b.extend_from_slice(&next.to_le_bytes());
    b
}

#[test]
fn unknown_field_type_is_preserved_verbatim() {
    // 0xF0 is not a recognised field type; the entry is preserved raw (issue #263 — nothing is
    // dropped on read → rewrite), not decoded and not skipped.
    let parsed = read(&minimal_le(0xF0, 0)).expect("parse with unknown type");
    assert_eq!(parsed.ifds.len(), 1);
    let fields = parsed.ifds[0].fields();
    assert_eq!(fields.len(), 1, "unknown-type field preserved");
    assert_eq!(fields[0].value.field_type(), None);
    assert_eq!(fields[0].value.type_code(), 0xF0);
    // It carries no decodable integer, so the typed accessor refuses.
    assert_eq!(parsed.ifds[0].get_u32(tags::IMAGE_WIDTH), None);
}

#[test]
fn known_field_parses() {
    let parsed = read(&minimal_le(3, 0)).expect("parse");
    assert_eq!(parsed.ifds[0].get_u32(tags::IMAGE_WIDTH), Some(4));
}

#[test]
fn ifd_loop_is_rejected() {
    // next-IFD offset points back at the first IFD (offset 8).
    assert!(read(&minimal_le(3, 8)).is_err());
}

#[test]
fn truncated_value_offset_is_rejected() {
    // First-IFD offset past end of file.
    let mut b = Vec::new();
    b.extend_from_slice(b"II");
    b.extend_from_slice(&42u16.to_le_bytes());
    b.extend_from_slice(&9999u32.to_le_bytes());
    assert!(read(&b).is_err());
}
