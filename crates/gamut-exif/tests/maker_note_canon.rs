//! The Canon `MakerNote` view against exiv2 (issue #977): the same TIFF stream decoded by
//! `CanonMakerNote` and by exiv2's `canonmn_int.cpp`, compared tag by tag through `exif_get`.
//!
//! The note is built here with file-absolute value offsets, as a Canon body writes it, and placed
//! by gamut's own writer; exiv2 then has to find it through `Make` and the Exif sub-IFD exactly as
//! it does in a camera file. Requires the `third_party/exiv2` + `third_party/expat` submodules and
//! a C++ toolchain.

use exiv2_oracle::exif_get;
use gamut_exif::{
    ByteOrder, CanonMakerNote, Exif, ExifTag, ExifWriter, FieldType, MakerNoteDialect, Value,
};

/// One IFD entry: tag, field type, count, and the value's bytes in the note's order.
struct Entry {
    tag: u16,
    ty: FieldType,
    count: u32,
    bytes: Vec<u8>,
}

fn shorts(tag: u16, values: &[u16], order: ByteOrder) -> Entry {
    let bytes = values.iter().flat_map(|&v| order.pack_u16(v)).collect();
    Entry {
        tag,
        ty: FieldType::Short,
        count: values.len() as u32,
        bytes,
    }
}

fn long(tag: u16, value: u32, order: ByteOrder) -> Entry {
    Entry {
        tag,
        ty: FieldType::Long,
        count: 1,
        bytes: order.pack_u32(value).to_vec(),
    }
}

fn ascii(tag: u16, text: &[u8]) -> Entry {
    Entry {
        tag,
        ty: FieldType::Ascii,
        count: text.len() as u32,
        bytes: text.to_vec(),
    }
}

/// `CameraSettings`: element 0 is the array's byte length (exiv2's "size element"); the rest stay
/// below element 23, where exiv2 starts merging three elements into `Exif.CanonCs.Lens`.
const CAMERA_SETTINGS: [u16; 10] = [20, 2, 0, 4, 0, 3, 1, 0, 0, 7];
/// `ShotInfo`, with its size element.
const SHOT_INFO: [u16; 6] = [12, 0, 160, 192, 0, 31];

/// A Canon note as a body writes it: a bare IFD at byte 0, a zero next-IFD pointer, and every
/// out-of-line value after it, addressed from the TIFF header with the note standing at
/// `note_at`. `OwnerName` is fixed-width and NUL-padded, as Canon writes it.
fn canon_note(order: ByteOrder, note_at: u32) -> Vec<u8> {
    let entries = [
        shorts(0x0001, &CAMERA_SETTINGS, order),
        shorts(0x0002, &[1, 105, 0, 0], order),
        shorts(0x0004, &SHOT_INFO, order),
        ascii(0x0006, b"Canon EOS R5\0"),
        ascii(0x0007, b"Firmware Version 1.8.1\0"),
        long(0x0008, 1_000_123, order),
        ascii(
            0x0009,
            b"Ada Lovelace\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        ),
        long(0x000c, 22_334_455, order),
        long(0x0010, 0x8000_0421, order),
        ascii(0x0095, b"RF24-105mm F4 L IS USM\0"),
        ascii(0x0096, b"XA1234567\0"),
    ];
    let count = entries.len() as u32;
    let mut data_at = 2 + 12 * count + 4;
    let (mut ifd, mut data) = (order.pack_u16(count as u16).to_vec(), Vec::new());
    for entry in &entries {
        ifd.extend_from_slice(&order.pack_u16(entry.tag));
        ifd.extend_from_slice(&order.pack_u16(entry.ty.code()));
        ifd.extend_from_slice(&order.pack_u32(entry.count));
        if entry.bytes.len() <= 4 {
            let mut word = entry.bytes.clone();
            word.resize(4, 0);
            ifd.extend_from_slice(&word);
        } else {
            ifd.extend_from_slice(&order.pack_u32(note_at + data_at));
            data.extend_from_slice(&entry.bytes);
            data_at += entry.bytes.len() as u32;
        }
    }
    ifd.extend_from_slice(&[0; 4]);
    ifd.extend_from_slice(&data);
    ifd
}

/// A bare TIFF stream whose Exif sub-IFD carries the Canon note, plus the note's position in it.
///
/// The position is not known until the writer lays the stream out, so a placeholder of the note's
/// length is written first, and the real note is then set into the parsed model, whose pin keeps it
/// at that same position (issue #263).
fn canon_stream(order: ByteOrder) -> (Vec<u8>, u64) {
    let writer = ExifWriter::new().marker(false);
    let mut exif = Exif::new(order);
    exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("Canon EOS R5".into()));
    let placeholder = vec![0; canon_note(order, 0).len()];
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(placeholder));
    let mut parsed = Exif::parse(&writer.write(&exif).expect("write")).expect("parse");
    let note_at = parsed.maker_note_offset().expect("an out-of-line note");
    let note = canon_note(order, u32::try_from(note_at).expect("small stream"));
    parsed.set_tag(ExifTag::MakerNote, Value::Undefined(note));
    let stream = writer.write(&parsed).expect("write");
    let reparsed = Exif::parse(&stream).expect("parse");
    assert_eq!(reparsed.maker_note_offset(), Some(note_at), "the pin held");
    (stream, note_at)
}

/// The view `MakerNote::canon` builds from the parsed model.
fn view(stream: &[u8]) -> CanonMakerNote {
    let exif = Exif::parse(stream).expect("parse");
    let note = exif.maker_note().expect("a note");
    let at = exif.maker_note_offset().expect("an offset");
    note.canon(exif.byte_order(), at)
        .expect("reads")
        .expect("a Canon note")
}

/// exiv2's value for `key` in `stream`.
fn exiv2(stream: &[u8], key: &str) -> String {
    exif_get(stream, key).unwrap_or_else(|error| panic!("exiv2 {key}: {error}"))
}

/// A `SHORT`/`LONG` array as exiv2 writes it: space-separated decimals.
fn joined<T: ToString>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn every_typed_accessor_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = canon_stream(order);
        let view = view(&stream);
        let text = [
            ("Exif.Canon.ImageType", view.image_type()),
            ("Exif.Canon.FirmwareVersion", view.firmware_version()),
            ("Exif.Canon.OwnerName", view.owner_name()),
            ("Exif.Canon.LensModel", view.lens_model()),
            (
                "Exif.Canon.InternalSerialNumber",
                view.internal_serial_number(),
            ),
        ];
        for (key, ours) in text {
            assert_eq!(Some(exiv2(&stream, key).as_str()), ours, "{key} {order:?}");
        }
        let numbers = [
            ("Exif.Canon.FileNumber", view.file_number()),
            ("Exif.Canon.SerialNumber", view.serial_number()),
            ("Exif.Canon.ModelID", view.model_id()),
        ];
        for (key, ours) in numbers {
            let ours = ours.map(|n| n.to_string());
            assert_eq!(Some(exiv2(&stream, key)), ours, "{key} {order:?}");
        }
        let focal = view.focal_length().expect("FocalLength");
        assert_eq!(exiv2(&stream, "Exif.Canon.FocalLength"), joined(focal));
    }
}

/// exiv2 splits the two binary arrays into one key per element (`Exif.CanonCs.0x0001`, …); the
/// view returns each array whole, and every element must agree.
#[test]
fn the_binary_arrays_agree_with_exiv2_element_by_element() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = canon_stream(order);
        let view = view(&stream);
        let arrays = [
            ("CanonCs", view.camera_settings().expect("CameraSettings")),
            ("CanonSi", view.shot_info().expect("ShotInfo")),
        ];
        for (group, ours) in arrays {
            for (index, value) in ours.iter().enumerate() {
                let key = format!("Exif.{group}.0x{index:04x}");
                assert_eq!(exiv2(&stream, &key), value.to_string(), "{key} {order:?}");
            }
        }
        assert_eq!(view.camera_settings(), Some(&CAMERA_SETTINGS[..]));
        assert_eq!(view.shot_info(), Some(&SHOT_INFO[..]));
    }
}

/// Every entry the view holds — not only the typed ones — is one exiv2 reads with the same value,
/// and none was unreadable.
#[test]
fn every_entry_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = canon_stream(order);
        let view = view(&stream);
        assert!(view.unreadable_tags().is_empty());
        assert_eq!(view.ifd().fields().len(), 11);
        for field in view.ifd().fields() {
            let ours = match &field.value {
                // exiv2 splits these two into the per-element keys checked above.
                Value::Short(_) if matches!(field.tag, 0x0001 | 0x0004) => continue,
                Value::Short(values) => joined(values),
                Value::Long(values) => joined(values),
                Value::Ascii(text) => text.split('\0').next().unwrap_or_default().to_owned(),
                other => panic!("unexpected value {other:?}"),
            };
            let key = format!("Exif.Canon.0x{:04x}", field.tag);
            assert_eq!(exiv2(&stream, &key), ours, "{key} {order:?}");
        }
    }
}

/// Reading through the whole stream, as exiv2 does, yields the view the note's bytes alone give.
#[test]
fn the_stream_and_the_note_alone_give_the_same_view() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, note_at) = canon_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let layout = exif.maker_note_layout().expect("a layout");
        assert_eq!(layout.dialect, MakerNoteDialect::Canon);
        let read = CanonMakerNote::read(&stream[..], note_at, layout).expect("reads");
        assert_eq!(read, Some(view(&stream)));
    }
}

/// Building the view changes nothing: the unmodified model re-emits the stream byte for byte.
#[test]
fn an_unmodified_canon_stream_round_trips_byte_identically() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = canon_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let _ = view(&stream);
        let rewritten = ExifWriter::new().marker(false).write(&exif).expect("write");
        assert_eq!(rewritten, stream);
    }
}
