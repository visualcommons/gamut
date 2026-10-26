//! The Fujifilm `MakerNote` view against exiv2 (issue #981): the same TIFF stream decoded by
//! `FujifilmMakerNote` and by exiv2's `fujimn_int.cpp`, compared tag by tag through `exif_get`.
//!
//! The note is built here as a Fujifilm body writes it — the `FUJIFILM` header, a little-endian
//! IFD at byte 12, value offsets counted from the note's start — and placed by gamut's own writer
//! in a stream of either byte order; exiv2 then has to find it through `Make` and the Exif sub-IFD
//! exactly as it does in a camera file. Requires the `third_party/exiv2` + `third_party/expat`
//! submodules and a C++ toolchain.

use exiv2_oracle::exif_get;
use gamut_exif::{
    ByteOrder, Exif, ExifTag, ExifWriter, FieldType, FujifilmMakerNote, MakerNoteDialect, Value,
};

/// The note's own byte order, fixed by the dialect.
const LE: ByteOrder = ByteOrder::LittleEndian;

/// One IFD entry: tag, field type, count, and the value's little-endian bytes.
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

fn short(tag: u16, value: u16) -> Entry {
    entry(tag, FieldType::Short, 1, LE.pack_u16(value).to_vec())
}

fn long(tag: u16, value: u32) -> Entry {
    entry(tag, FieldType::Long, 1, LE.pack_u32(value).to_vec())
}

fn slongs(tag: u16, values: &[i32]) -> Entry {
    let bytes = values
        .iter()
        .flat_map(|&v| LE.pack_u32(v.cast_unsigned()))
        .collect();
    entry(tag, FieldType::SLong, values.len(), bytes)
}

fn rational(tag: u16, (n, d): (u32, u32)) -> Entry {
    let mut bytes = LE.pack_u32(n).to_vec();
    bytes.extend_from_slice(&LE.pack_u32(d));
    entry(tag, FieldType::Rational, 1, bytes)
}

fn srational(tag: u16, (n, d): (i32, i32)) -> Entry {
    let mut bytes = LE.pack_u32(n.cast_unsigned()).to_vec();
    bytes.extend_from_slice(&LE.pack_u32(d.cast_unsigned()));
    entry(tag, FieldType::SRational, 1, bytes)
}

fn undefined(tag: u16, values: &[u8]) -> Entry {
    entry(tag, FieldType::Undefined, values.len(), values.to_vec())
}

fn ascii(tag: u16, text: &[u8]) -> Entry {
    entry(tag, FieldType::Ascii, text.len(), text.to_vec())
}

/// The thirty-three typed tags of `references/exif/fujifilm/README.md`, with values a Fujifilm
/// body could write (negative where the type is signed, so a sign error is visible).
fn entries() -> Vec<Entry> {
    vec![
        undefined(0x0000, b"0130"),
        ascii(0x0010, b"FC  A1234567\0"),
        ascii(0x1000, b"FINE\0\0\0\0"),
        short(0x1001, 3),
        short(0x1002, 0x100),
        short(0x1003, 0x200),
        short(0x1004, 0x300),
        short(0x1005, 5_600),
        short(0x1006, 0x400),
        slongs(0x100a, &[-40, 60]),
        short(0x100b, 0x40),
        short(0x100e, 0x280),
        short(0x1010, 0x8120),
        srational(0x1011, (-2, 3)),
        short(0x1020, 1),
        short(0x1021, 2),
        short(0x1030, 4),
        short(0x1031, 5),
        slongs(0x1040, &[-16]),
        slongs(0x1041, &[32]),
        short(0x1100, 6),
        short(0x1101, 7),
        short(0x1300, 8),
        short(0x1301, 9),
        short(0x1302, 10),
        short(0x1400, 11),
        short(0x1401, 0x600),
        short(0x1402, 0x201),
        rational(0x1404, (18, 1)),
        rational(0x1405, (55, 1)),
        rational(0x1406, (28, 10)),
        rational(0x1407, (4, 1)),
        long(0x1431, 3_000),
    ]
}

/// A Fujifilm note as a body writes it: `FUJIFILM`, the IFD's offset (12) as a little-endian
/// `u32`, the IFD with a zero next-IFD pointer, and every out-of-line value after it, addressed
/// from the note's start.
fn fujifilm_note() -> Vec<u8> {
    let entries = entries();
    let count = u32::try_from(entries.len()).expect("small");
    let mut data_at = 12 + 2 + 12 * count + 4;
    let mut note = b"FUJIFILM".to_vec();
    note.extend_from_slice(&LE.pack_u32(12));
    note.extend_from_slice(&LE.pack_u16(u16::try_from(count).expect("small")));
    let mut data = Vec::new();
    for entry in &entries {
        note.extend_from_slice(&LE.pack_u16(entry.tag));
        note.extend_from_slice(&LE.pack_u16(entry.ty.code()));
        note.extend_from_slice(&LE.pack_u32(entry.count));
        if entry.bytes.len() <= 4 {
            let mut word = entry.bytes.clone();
            word.resize(4, 0);
            note.extend_from_slice(&word);
        } else {
            note.extend_from_slice(&LE.pack_u32(data_at));
            data.extend_from_slice(&entry.bytes);
            data_at += u32::try_from(entry.bytes.len()).expect("small");
        }
    }
    note.extend_from_slice(&[0; 4]);
    note.extend_from_slice(&data);
    note
}

/// A bare TIFF stream of byte order `order` whose Exif sub-IFD carries the Fujifilm note.
fn fujifilm_stream(order: ByteOrder) -> Vec<u8> {
    let writer = ExifWriter::new().marker(false);
    let mut exif = Exif::new(order);
    exif.set_tag(ExifTag::Make, Value::Ascii("FUJIFILM".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("X-T5".into()));
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(fujifilm_note()));
    writer.write(&exif).expect("write")
}

/// The view `MakerNote::fujifilm` builds from the parsed model.
fn view(stream: &[u8]) -> FujifilmMakerNote {
    let exif = Exif::parse(stream).expect("parse");
    let note = exif.maker_note().expect("a note");
    let at = exif.maker_note_offset().expect("an offset");
    note.fujifilm(exif.byte_order(), at)
        .expect("reads")
        .expect("a Fujifilm note")
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
/// `references/exif/fujifilm/README.md` are checked too.
#[test]
fn every_typed_accessor_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let stream = fujifilm_stream(order);
        let view = view(&stream);
        let n = |value: Option<u16>| value.map(|n| n.to_string());
        let r = |value: Option<gamut_exif::Rational>| value.map(|r| format!("{}/{}", r.num, r.den));
        let ours = [
            ("Version", view.version().map(joined)),
            ("SerialNumber", view.serial_number().map(str::to_owned)),
            ("Quality", view.quality().map(str::to_owned)),
            ("Sharpness", n(view.sharpness())),
            ("WhiteBalance", n(view.white_balance())),
            ("Color", n(view.color())),
            ("Tone", n(view.tone())),
            ("ColorTemperature", n(view.color_temperature())),
            ("Contrast", n(view.contrast())),
            (
                "WhiteBalanceFineTune",
                view.white_balance_fine_tune().map(joined),
            ),
            ("NoiseReduction", n(view.noise_reduction())),
            ("HighIsoNoiseReduction", n(view.high_iso_noise_reduction())),
            ("FlashMode", n(view.flash_mode())),
            (
                "FlashStrength",
                view.flash_strength()
                    .map(|r| format!("{}/{}", r.num, r.den)),
            ),
            ("Macro", n(view.macro_mode())),
            ("FocusMode", n(view.focus_mode())),
            ("SlowSync", n(view.slow_sync())),
            ("PictureMode", n(view.picture_mode())),
            ("ShadowTone", view.shadow_tone().map(|n| n.to_string())),
            (
                "HighlightTone",
                view.highlight_tone().map(|n| n.to_string()),
            ),
            ("Continuous", n(view.continuous())),
            ("SequenceNumber", n(view.sequence_number())),
            ("BlurWarning", n(view.blur_warning())),
            ("FocusWarning", n(view.focus_warning())),
            ("ExposureWarning", n(view.exposure_warning())),
            ("DynamicRange", n(view.dynamic_range())),
            ("FilmMode", n(view.film_mode())),
            ("DynamicRangeSetting", n(view.dynamic_range_setting())),
            ("MinFocalLength", r(view.min_focal_length())),
            ("MaxFocalLength", r(view.max_focal_length())),
            ("MaxApertureAtMinFocal", r(view.max_aperture_at_min_focal())),
            ("MaxApertureAtMaxFocal", r(view.max_aperture_at_max_focal())),
            ("Rating", view.rating().map(|n| n.to_string())),
        ];
        for (name, ours) in ours {
            let key = format!("Exif.Fujifilm.{name}");
            assert_eq!(Some(exiv2(&stream, &key)), ours, "{key} {order:?}");
        }
    }
}

/// Every entry the view holds is one exiv2 reads with the same value, and none was unreadable.
#[test]
fn every_entry_agrees_with_exiv2() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let stream = fujifilm_stream(order);
        let view = view(&stream);
        assert!(view.unreadable_tags().is_empty());
        assert_eq!(view.ifd().fields().len(), 33);
        for field in view.ifd().fields() {
            let ours = match &field.value {
                Value::Undefined(values) => joined(values),
                Value::Short(values) => joined(values),
                Value::Long(values) => joined(values),
                Value::SLong(values) => joined(values),
                Value::Rational(values) => values
                    .iter()
                    .map(|(n, d)| format!("{n}/{d}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                Value::SRational(values) => values
                    .iter()
                    .map(|(n, d)| format!("{n}/{d}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                Value::Ascii(text) => text.split('\0').next().unwrap_or_default().to_owned(),
                other => panic!("unexpected value {other:?}"),
            };
            let key = format!("Exif.Fujifilm.0x{:04x}", field.tag);
            assert_eq!(exiv2(&stream, &key), ours, "{key} {order:?}");
        }
    }
}

/// Reading through the whole stream, as exiv2 does, yields the view the note's bytes alone give.
#[test]
fn the_stream_and_the_note_alone_give_the_same_view() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let stream = fujifilm_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let layout = exif.maker_note_layout().expect("a layout");
        assert_eq!(layout.dialect, MakerNoteDialect::Fujifilm);
        assert_eq!(layout.byte_order, LE);
        let at = exif.maker_note_offset().expect("an offset");
        let read = FujifilmMakerNote::read(&stream[..], at, layout).expect("reads");
        assert_eq!(read, Some(view(&stream)));
    }
}

/// Building the view changes nothing: the unmodified model re-emits the stream byte for byte.
#[test]
fn an_unmodified_fujifilm_stream_round_trips_byte_identically() {
    for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        let stream = fujifilm_stream(order);
        let exif = Exif::parse(&stream).expect("parse");
        let _ = view(&stream);
        let rewritten = ExifWriter::new().marker(false).write(&exif).expect("write");
        assert_eq!(rewritten, stream);
    }
}
