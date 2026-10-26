//! The Sony `MakerNote` view against exiv2 (issue #980): the same TIFF stream decoded by
//! `SonyMakerNote` and by exiv2's `sonymn_int.cpp`, compared tag by tag through `exif_get`.
//!
//! The note is a `Sony2` note (a bare IFD, no `SONY DSC` header) built here with file-absolute
//! value offsets, as a Sony body writes it, and placed by gamut's own writer; exiv2 then has to
//! find it through `Make` and the Exif sub-IFD exactly as it does in a camera file. Requires the
//! `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use exiv2_oracle::exif_get;
use gamut_exif::{
    ByteOrder, Exif, ExifTag, ExifWriter, FieldType, MakerNoteDialect, SonyMakerNote, Value,
};

/// One IFD entry: tag, field type, count, and the value's bytes in the note's order.
struct Entry {
    tag: u16,
    ty: FieldType,
    count: u32,
    bytes: Vec<u8>,
}

fn entry(tag: u16, ty: FieldType, count: usize, bytes: Vec<u8>) -> Entry {
    Entry {
        tag,
        ty,
        count: u32::try_from(count).expect("small"),
        bytes,
    }
}

fn longs(tag: u16, values: &[u32], order: ByteOrder) -> Entry {
    let bytes = values.iter().flat_map(|&v| order.pack_u32(v)).collect();
    entry(tag, FieldType::Long, values.len(), bytes)
}

fn slong(tag: u16, value: i32, order: ByteOrder) -> Entry {
    let bytes = order.pack_u32(value.cast_unsigned()).to_vec();
    entry(tag, FieldType::SLong, 1, bytes)
}

fn short(tag: u16, value: u16, order: ByteOrder) -> Entry {
    entry(tag, FieldType::Short, 1, order.pack_u16(value).to_vec())
}

fn srational(tag: u16, (n, d): (i32, i32), order: ByteOrder) -> Entry {
    let mut bytes = order.pack_u32(n.cast_unsigned()).to_vec();
    bytes.extend_from_slice(&order.pack_u32(d.cast_unsigned()));
    entry(tag, FieldType::SRational, 1, bytes)
}

fn bytes(tag: u16, values: &[u8]) -> Entry {
    entry(tag, FieldType::Byte, values.len(), values.to_vec())
}

fn ascii(tag: u16, text: &[u8]) -> Entry {
    entry(tag, FieldType::Ascii, text.len(), text.to_vec())
}

/// The sixteen typed tags of `references/exif/sony/README.md`, with values a Sony body could
/// write (negative where the type is signed, so a sign error is visible).
fn entries(order: ByteOrder) -> Vec<Entry> {
    vec![
        longs(0x0102, &[2], order),
        srational(0x0104, (-2, 3), order),
        longs(0x0115, &[16], order),
        longs(0x2002, &[4], order),
        slong(0x2004, -1, order),
        slong(0x2005, 2, order),
        slong(0x2006, -3, order),
        slong(0x2007, 5, order),
        ascii(0x2031, b"04523117\0\0\0\0"),
        bytes(0xb000, &[4, 0, 0, 0]),
        short(0xb001, 358, order),
        ascii(0xb020, b"Standard\0"),
        longs(0xb021, &[5_500], order),
        longs(0xb027, &[65_535], order),
        bytes(0xb02a, &[0, 0x24, 0x70, 0x28, 0x28, 0, 0x80, 0]),
        longs(0xb02b, &[4_000, 6_000], order),
    ]
}

/// A `Sony2` note as a body writes it: a bare IFD at byte 0, a zero next-IFD pointer, and every
/// out-of-line value after it, addressed from the TIFF header with the note standing at
/// `note_at`.
fn sony_note(order: ByteOrder, note_at: u32) -> Vec<u8> {
    let entries = entries(order);
    let count = u32::try_from(entries.len()).expect("small");
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
            data_at += u32::try_from(entry.bytes.len()).expect("small");
        }
    }
    ifd.extend_from_slice(&[0; 4]);
    ifd.extend_from_slice(&data);
    ifd
}

/// A bare TIFF stream whose Exif sub-IFD carries the Sony note, plus the note's position in it.
///
/// The position is not known until the writer lays the stream out, so a placeholder of the note's
/// length is written first, and the real note is then set into the parsed model, whose pin keeps it
/// at that same position (issue #263).
fn sony_stream(order: ByteOrder) -> (Vec<u8>, u64) {
    let writer = ExifWriter::new().marker(false);
    let mut exif = Exif::new(order);
    exif.set_tag(ExifTag::Make, Value::Ascii("SONY".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("ILCE-9".into()));
    let placeholder = vec![0; sony_note(order, 0).len()];
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(placeholder));
    let mut parsed = Exif::parse(&writer.write(&exif).expect("write")).expect("parse");
    let note_at = parsed.maker_note_offset().expect("an out-of-line note");
    let note = sony_note(order, u32::try_from(note_at).expect("small stream"));
    parsed.set_tag(ExifTag::MakerNote, Value::Undefined(note));
    let stream = writer.write(&parsed).expect("write");
    let reparsed = Exif::parse(&stream).expect("parse");
    assert_eq!(reparsed.maker_note_offset(), Some(note_at), "the pin held");
    (stream, note_at)
}

/// The view `MakerNote::sony` builds from the parsed model.
fn view(stream: &[u8]) -> SonyMakerNote {
    let exif = Exif::parse(stream).expect("parse");
    let note = exif.maker_note().expect("a note");
    let at = exif.maker_note_offset().expect("an offset");
    note.sony(exif.byte_order(), at)
        .expect("reads")
        .expect("a Sony note")
}

/// exiv2's value for `key` in `stream`.
fn exiv2(stream: &[u8], key: &str) -> String {
    exif_get(stream, key).unwrap_or_else(|error| panic!("exiv2 {key}: {error}"))
}

/// An array as exiv2 writes it: space-separated decimals.
fn joined<T: ToString>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Each accessor against exiv2's key of the staked name, so the names in
/// `references/exif/sony/README.md` are checked too.
#[test]
fn every_typed_accessor_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = sony_stream(order);
        let view = view(&stream);
        let ours = [
            ("Quality", view.quality().map(|n| n.to_string())),
            (
                "FlashExposureComp",
                view.flash_exposure_comp()
                    .map(|r| format!("{}/{}", r.num, r.den)),
            ),
            ("WhiteBalance", view.white_balance().map(|n| n.to_string())),
            ("Rating", view.rating().map(|n| n.to_string())),
            ("Contrast", view.contrast().map(|n| n.to_string())),
            ("Saturation", view.saturation().map(|n| n.to_string())),
            ("Sharpness", view.sharpness().map(|n| n.to_string())),
            ("Brightness", view.brightness().map(|n| n.to_string())),
            ("SerialNumber", view.serial_number().map(str::to_owned)),
            ("FileFormat", view.file_format().map(joined)),
            ("SonyModelID", view.model_id().map(|n| n.to_string())),
            ("CreativeStyle", view.creative_style().map(str::to_owned)),
            (
                "ColorTemperature",
                view.color_temperature().map(|n| n.to_string()),
            ),
            ("LensID", view.lens_id().map(|n| n.to_string())),
            ("LensSpec", view.lens_spec().map(joined)),
            ("FullImageSize", view.full_image_size().map(joined)),
        ];
        for (name, ours) in ours {
            let key = format!("Exif.Sony2.{name}");
            assert_eq!(Some(exiv2(&stream, &key)), ours, "{key} {order:?}");
        }
    }
}

/// Every entry the view holds is one exiv2 reads with the same value, and none was unreadable.
#[test]
fn every_entry_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = sony_stream(order);
        let view = view(&stream);
        assert!(view.unreadable_tags().is_empty());
        assert_eq!(view.ifd().fields().len(), 16);
        for field in view.ifd().fields() {
            let ours = match &field.value {
                Value::Byte(values) => joined(values),
                Value::Short(values) => joined(values),
                Value::Long(values) => joined(values),
                Value::SLong(values) => joined(values),
                Value::SRational(values) => values
                    .iter()
                    .map(|(n, d)| format!("{n}/{d}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                Value::Ascii(text) => text.split('\0').next().unwrap_or_default().to_owned(),
                other => panic!("unexpected value {other:?}"),
            };
            let key = format!("Exif.Sony2.0x{:04x}", field.tag);
            assert_eq!(exiv2(&stream, &key), ours, "{key} {order:?}");
        }
    }
}

/// Reading through the whole stream, as exiv2 does, yields the view the note's bytes alone give.
#[test]
fn the_stream_and_the_note_alone_give_the_same_view() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, note_at) = sony_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let layout = exif.maker_note_layout().expect("a layout");
        assert_eq!(layout.dialect, MakerNoteDialect::Sony2);
        let read = SonyMakerNote::read(&stream[..], note_at, layout).expect("reads");
        assert_eq!(read, Some(view(&stream)));
    }
}

/// Building the view changes nothing: the unmodified model re-emits the stream byte for byte.
#[test]
fn an_unmodified_sony_stream_round_trips_byte_identically() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let (stream, _) = sony_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let _ = view(&stream);
        let rewritten = ExifWriter::new().marker(false).write(&exif).expect("write");
        assert_eq!(rewritten, stream);
    }
}
