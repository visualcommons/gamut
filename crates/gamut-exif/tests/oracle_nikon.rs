//! The Nikon `MakerNote` view against exiv2 (issue #979): the same TIFF stream decoded by
//! `NikonMakerNote` and by exiv2's `nikonmn_int.cpp`, compared tag by tag through `exif_get`.
//!
//! The corpus holds no Nikon file, so each note is generated here as a Nikon body writes it — a
//! `Nikon3` note with its embedded TIFF header and note-relative offsets, and the older `Nikon1`
//! and `Nikon2` notes with file-absolute ones — and placed by gamut's own writer; exiv2 then has to
//! find it through `Make` and the Exif sub-IFD exactly as it does in a camera file. Requires the
//! `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use exiv2_oracle::exif_get;
use gamut_exif::{
    ByteOrder, Exif, ExifTag, ExifWriter, FieldType, MakerNoteDialect, NikonMakerNote, Rational,
    SRational, Value,
};

const ORDERS: [ByteOrder; 2] = [ByteOrder::LittleEndian, ByteOrder::BigEndian];

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

fn shorts(tag: u16, values: &[u16], order: ByteOrder) -> Entry {
    let bytes = values.iter().flat_map(|&v| order.pack_u16(v)).collect();
    entry(tag, FieldType::Short, values.len(), bytes)
}

fn sshorts(tag: u16, values: &[i16], order: ByteOrder) -> Entry {
    let bytes = values
        .iter()
        .flat_map(|&v| order.pack_u16(v.cast_unsigned()))
        .collect();
    entry(tag, FieldType::SShort, values.len(), bytes)
}

fn long(tag: u16, value: u32, order: ByteOrder) -> Entry {
    entry(tag, FieldType::Long, 1, order.pack_u32(value).to_vec())
}

fn rationals(tag: u16, values: &[(u32, u32)], order: ByteOrder) -> Entry {
    let bytes = values
        .iter()
        .flat_map(|&(n, d)| [order.pack_u32(n), order.pack_u32(d)])
        .flatten()
        .collect();
    entry(tag, FieldType::Rational, values.len(), bytes)
}

fn srational(tag: u16, (n, d): (i32, i32), order: ByteOrder) -> Entry {
    let mut bytes = order.pack_u32(n.cast_unsigned()).to_vec();
    bytes.extend_from_slice(&order.pack_u32(d.cast_unsigned()));
    entry(tag, FieldType::SRational, 1, bytes)
}

fn byte(tag: u16, value: u8) -> Entry {
    entry(tag, FieldType::Byte, 1, vec![value])
}

fn undefined(tag: u16, values: &[u8]) -> Entry {
    entry(tag, FieldType::Undefined, values.len(), values.to_vec())
}

fn ascii(tag: u16, text: &[u8]) -> Entry {
    entry(tag, FieldType::Ascii, text.len(), text.to_vec())
}

/// A record: its four version bytes, then `len - 4` bytes of payload standing in for what a body
/// enciphers (exiv2 deciphers them; the version stays plain).
fn record(tag: u16, version: &[u8; 4], len: usize) -> Entry {
    let mut bytes = version.to_vec();
    bytes.extend((4..len).map(|i| u8::try_from(i % 251).expect("below 251")));
    undefined(tag, &bytes)
}

/// The records' tags, which exiv2 splits into binary arrays rather than keeping as entries.
const RECORDS: [u16; 3] = [0x0091, 0x0097, 0x0098];

/// The thirty-eight `Nikon3Tags` of `references/exif/nikon/README.md`, with values a Nikon body
/// could write (negative where the type is signed, so a sign error is visible). The serial number
/// and shutter count are the keys exiv2 deciphers the records with.
fn nikon3_entries(order: ByteOrder) -> Vec<Entry> {
    vec![
        undefined(0x0001, b"0211"),
        shorts(0x0002, &[0, 400], order),
        ascii(0x0003, b"COLOR\0"),
        ascii(0x0004, b"FINE   \0"),
        ascii(0x0005, b"AUTO1\0"),
        ascii(0x0006, b"NORMAL\0"),
        ascii(0x0007, b"AF-S  \0"),
        ascii(0x0008, b"NORMAL\0"),
        ascii(0x0009, b"SB-800\0"),
        sshorts(0x000b, &[-3, 0], order),
        rationals(0x000c, &[(512, 256), (397, 256), (1, 1), (1, 1)], order),
        ascii(0x000f, b"MANUAL\0"),
        srational(0x0019, (-1, 3), order),
        ascii(0x001d, b"3001234\0"),
        shorts(0x001e, &[2], order),
        shorts(0x0022, &[3], order),
        shorts(0x002a, &[1], order),
        ascii(0x0080, b"AUTO\0"),
        ascii(0x0081, b"HIGH\0"),
        ascii(0x0082, b"OFF\0"),
        byte(0x0083, 14),
        rationals(0x0084, &[(180, 10), (550, 10), (35, 10), (56, 10)], order),
        rationals(0x0085, &[(7, 2)], order),
        rationals(0x0086, &[(9, 4)], order),
        byte(0x0087, 9),
        shorts(0x0089, &[16], order),
        ascii(0x008f, b"PORTRAIT\0"),
        ascii(0x0090, b"NATURAL\0"),
        record(0x0091, b"0221", 640),
        sshorts(0x0092, &[-9], order),
        shorts(0x0093, &[4], order),
        ascii(0x0095, b"FPNR\0"),
        record(0x0097, b"0100", 80),
        record(0x0098, b"0204", 20),
        long(0x00a5, 1_234, order),
        long(0x00a7, 56_789, order),
        ascii(0x00ac, b"VR-ON\0"),
        shorts(0x00b1, &[6], order),
    ]
}

/// The thirteen tags exiv2's `Nikon1` table shares with `Nikon3`.
fn nikon1_entries(order: ByteOrder) -> Vec<Entry> {
    vec![
        undefined(0x0001, b"0100"),
        shorts(0x0002, &[0, 100], order),
        ascii(0x0003, b"COLOR\0"),
        ascii(0x0004, b"FINE\0"),
        ascii(0x0005, b"AUTO\0"),
        ascii(0x0006, b"AUTO\0"),
        ascii(0x0007, b"AF-C\0"),
        ascii(0x0008, b"NORMAL\0"),
        ascii(0x000f, b"AUTO\0"),
        ascii(0x0080, b"NORMAL\0"),
        ascii(0x0082, b"WIDE\0"),
        rationals(0x0085, &[(0, 1)], order),
        rationals(0x0086, &[(3, 2)], order),
    ]
}

/// The eight `Nikon2Tags`.
fn nikon2_entries(order: ByteOrder) -> Vec<Entry> {
    vec![
        shorts(0x0003, &[2], order),
        shorts(0x0004, &[1], order),
        shorts(0x0005, &[3], order),
        shorts(0x0006, &[5], order),
        shorts(0x0007, &[6], order),
        rationals(0x0008, &[(11, 10)], order),
        rationals(0x000a, &[(15, 10)], order),
        shorts(0x000b, &[4], order),
    ]
}

/// `entries` as an IFD with a zero next-IFD pointer and every out-of-line value after it,
/// addressed as if the IFD stood at `at` in the offset space its offsets count in.
fn ifd(entries: &[Entry], order: ByteOrder, at: u32) -> Vec<u8> {
    let count = u32::try_from(entries.len()).expect("small");
    let mut data_at = at + 2 + 12 * count + 4;
    let (mut ifd, mut data) = (order.pack_u16(count as u16).to_vec(), Vec::new());
    for entry in entries {
        ifd.extend_from_slice(&order.pack_u16(entry.tag));
        ifd.extend_from_slice(&order.pack_u16(entry.ty.code()));
        ifd.extend_from_slice(&order.pack_u32(entry.count));
        if entry.bytes.len() <= 4 {
            let mut word = entry.bytes.clone();
            word.resize(4, 0);
            ifd.extend_from_slice(&word);
        } else {
            ifd.extend_from_slice(&order.pack_u32(data_at));
            data.extend_from_slice(&entry.bytes);
            data_at += u32::try_from(entry.bytes.len()).expect("small");
        }
    }
    ifd.extend_from_slice(&[0; 4]);
    ifd.extend_from_slice(&data);
    ifd
}

/// The dialects, each with its note builder: given the note's order and its position in the
/// stream, the note's bytes.
fn note(dialect: MakerNoteDialect, order: ByteOrder, note_at: u32) -> Vec<u8> {
    match dialect {
        MakerNoteDialect::Nikon3 => {
            let mut note = b"Nikon\0\x02\x10\0\0".to_vec();
            note.extend_from_slice(match order {
                ByteOrder::LittleEndian => b"II*\0\x08\0\0\0",
                ByteOrder::BigEndian => b"MM\0*\0\0\0\x08",
            });
            note.extend_from_slice(&ifd(&nikon3_entries(order), order, 8));
            note
        }
        MakerNoteDialect::Nikon2 => {
            let mut note = b"Nikon\0\x01\0".to_vec();
            note.extend_from_slice(&ifd(&nikon2_entries(order), order, note_at + 8));
            note
        }
        _ => ifd(&nikon1_entries(order), order, note_at),
    }
}

/// A bare TIFF stream of byte order `outer` whose Exif sub-IFD carries a `dialect` note in the
/// note order `order`, plus the note's position in it.
///
/// A `Nikon1` or `Nikon2` note's offsets are file-absolute, so its position must be known before
/// it is built: a placeholder of its length is written first, and the real note is then set into
/// the parsed model, whose pin keeps it at that same position (issue #263).
fn nikon_stream(dialect: MakerNoteDialect, outer: ByteOrder, order: ByteOrder) -> (Vec<u8>, u64) {
    let writer = ExifWriter::new().with_marker(false);
    let mut exif = Exif::new(outer);
    exif.set_tag(ExifTag::Make, Value::Ascii("NIKON CORPORATION".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("NIKON D300".into()));
    let placeholder = vec![0; note(dialect, order, 0).len()];
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(placeholder));
    let mut parsed = Exif::parse(&writer.write(&exif).expect("write")).expect("parse");
    let note_at = parsed.maker_note_offset().expect("an out-of-line note");
    let bytes = note(
        dialect,
        order,
        u32::try_from(note_at).expect("small stream"),
    );
    parsed.set_tag(ExifTag::MakerNote, Value::Undefined(bytes));
    let stream = writer.write(&parsed).expect("write");
    let reparsed = Exif::parse(&stream).expect("parse");
    assert_eq!(reparsed.maker_note_offset(), Some(note_at), "the pin held");
    (stream, note_at)
}

/// Every dialect, in both note orders (inherited from the outer stream for `Nikon1`/`Nikon2`, the
/// opposite of the outer stream's for `Nikon3`, whose embedded header decides).
fn streams() -> Vec<(MakerNoteDialect, ByteOrder, Vec<u8>, u64)> {
    let mut streams = Vec::new();
    for order in ORDERS {
        for dialect in [
            MakerNoteDialect::Nikon1,
            MakerNoteDialect::Nikon2,
            MakerNoteDialect::Nikon3,
        ] {
            let outer = match (dialect, order) {
                (MakerNoteDialect::Nikon3, ByteOrder::LittleEndian) => ByteOrder::BigEndian,
                (MakerNoteDialect::Nikon3, ByteOrder::BigEndian) => ByteOrder::LittleEndian,
                _ => order,
            };
            let (stream, at) = nikon_stream(dialect, outer, order);
            streams.push((dialect, order, stream, at));
        }
    }
    streams
}

/// The view `MakerNote::nikon` builds from the parsed model.
fn view(stream: &[u8]) -> NikonMakerNote {
    let exif = Exif::parse(stream).expect("parse");
    let note = exif.maker_note().expect("a note");
    let at = exif.maker_note_offset().expect("an offset");
    note.nikon(exif.byte_order(), at)
        .expect("reads")
        .expect("a Nikon note")
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

fn ratio(values: &[(u32, u32)]) -> String {
    joined(
        &values
            .iter()
            .map(|(n, d)| format!("{n}/{d}"))
            .collect::<Vec<_>>(),
    )
}

fn n<T: ToString>(value: Option<T>) -> Option<String> {
    value.map(|value| value.to_string())
}

fn r(value: Option<Rational>) -> Option<String> {
    value.map(|r| format!("{}/{}", r.num, r.den))
}

/// Each `Nikon3Tags` accessor against exiv2's key of the staked name, so the names in
/// `references/exif/nikon/README.md` are checked too; each record's version against exiv2's
/// reading of the record it selected (and, for `0221` and `0204`, deciphered).
#[test]
fn every_nikon3_accessor_agrees_with_exiv2() {
    for (dialect, order, stream, _) in streams() {
        if dialect != MakerNoteDialect::Nikon3 {
            continue;
        }
        let view = view(&stream);
        let tags = view.nikon3().expect("Nikon3 tags");
        let s = |value: Option<&str>| value.map(str::to_owned);
        let sr = |value: Option<SRational>| value.map(|r| format!("{}/{}", r.num, r.den));
        let ours = [
            ("Version", tags.version().map(joined)),
            ("ISOSpeed", tags.iso_speed().map(joined)),
            ("ColorMode", s(tags.color_mode())),
            ("Quality", s(tags.quality())),
            ("WhiteBalance", s(tags.white_balance())),
            ("Sharpening", s(tags.sharpening())),
            ("Focus", s(tags.focus())),
            ("FlashSetting", s(tags.flash_setting())),
            ("FlashDevice", s(tags.flash_device())),
            ("WhiteBalanceBias", tags.white_balance_bias().map(joined)),
            ("WB_RBLevels", tags.wb_rb_levels().map(ratio)),
            ("ISOSelection", s(tags.iso_selection())),
            ("ExposureBracketComp", sr(tags.exposure_bracket_comp())),
            ("SerialNumber", s(tags.serial_number())),
            ("ColorSpace", n(tags.color_space())),
            ("ActiveDLighting", n(tags.active_d_lighting())),
            ("VignetteControl", n(tags.vignette_control())),
            ("ImageAdjustment", s(tags.image_adjustment())),
            ("ToneComp", s(tags.tone_comp())),
            ("AuxiliaryLens", s(tags.auxiliary_lens())),
            ("LensType", n(tags.lens_type())),
            ("Lens", tags.lens().map(ratio)),
            ("FocusDistance", r(tags.focus_distance())),
            ("DigitalZoom", r(tags.digital_zoom())),
            ("FlashMode", n(tags.flash_mode())),
            ("ShootingMode", n(tags.shooting_mode())),
            ("SceneMode", s(tags.scene_mode())),
            ("LightSource", s(tags.light_source())),
            ("HueAdjustment", n(tags.hue_adjustment())),
            ("NEFCompression", n(tags.nef_compression())),
            ("NoiseReduction", s(tags.noise_reduction())),
            ("ImageCount", n(tags.image_count())),
            ("ShutterCount", n(tags.shutter_count())),
            ("ImageStabilization", s(tags.image_stabilization())),
            ("HighISONoiseReduction", n(tags.high_iso_noise_reduction())),
        ];
        for (name, ours) in ours {
            let key = format!("Exif.Nikon3.{name}");
            assert_eq!(Some(exiv2(&stream, &key)), ours, "{key} {order:?}");
        }
        let records = [
            ("NikonSi02xx", tags.shot_info()),
            ("NikonCb1", tags.color_balance()),
            ("NikonLd3", tags.lens_data()),
        ];
        for (group, record) in records {
            let key = format!("Exif.{group}.Version");
            let ours = record.map(|record| joined(&record.version()));
            assert_eq!(Some(exiv2(&stream, &key)), ours, "{key} {order:?}");
        }
    }
}

/// The `Nikon1` and `Nikon2` accessors against exiv2's keys of the staked names.
#[test]
fn every_nikon1_and_nikon2_accessor_agrees_with_exiv2() {
    for (dialect, order, stream, _) in streams() {
        let view = view(&stream);
        let ours = match dialect {
            MakerNoteDialect::Nikon1 => {
                let tags = view.nikon3().expect("Nikon3 tags");
                let s = |value: Option<&str>| value.map(str::to_owned);
                vec![
                    ("Version", tags.version().map(joined)),
                    ("ISOSpeed", tags.iso_speed().map(joined)),
                    ("ColorMode", s(tags.color_mode())),
                    ("Quality", s(tags.quality())),
                    ("WhiteBalance", s(tags.white_balance())),
                    ("Sharpening", s(tags.sharpening())),
                    ("Focus", s(tags.focus())),
                    ("FlashSetting", s(tags.flash_setting())),
                    ("ISOSelection", s(tags.iso_selection())),
                    ("ImageAdjustment", s(tags.image_adjustment())),
                    ("AuxiliaryLens", s(tags.auxiliary_lens())),
                    ("FocusDistance", r(tags.focus_distance())),
                    ("DigitalZoom", r(tags.digital_zoom())),
                ]
            }
            MakerNoteDialect::Nikon2 => {
                let tags = view.nikon2().expect("Nikon2 tags");
                vec![
                    ("Quality", n(tags.quality())),
                    ("ColorMode", n(tags.color_mode())),
                    ("ImageAdjustment", n(tags.image_adjustment())),
                    ("ISOSpeed", n(tags.iso_speed())),
                    ("WhiteBalance", n(tags.white_balance())),
                    ("Focus", r(tags.focus())),
                    ("DigitalZoom", r(tags.digital_zoom())),
                    ("AuxiliaryLens", n(tags.auxiliary_lens())),
                ]
            }
            _ => continue,
        };
        for (name, ours) in ours {
            let key = format!("Exif.{dialect:?}.{name}");
            assert_eq!(Some(exiv2(&stream, &key)), ours, "{key} {order:?}");
        }
    }
}

/// Every entry the view holds is one exiv2 reads with the same value — the records excepted,
/// which exiv2 splits into binary arrays — and none was unreadable.
#[test]
fn every_entry_agrees_with_exiv2() {
    for (dialect, order, stream, _) in streams() {
        let view = view(&stream);
        assert_eq!(view.dialect(), dialect);
        assert!(view.unreadable_tags().is_empty(), "{dialect:?} {order:?}");
        let want = match dialect {
            MakerNoteDialect::Nikon1 => 13,
            MakerNoteDialect::Nikon2 => 8,
            _ => 38,
        };
        assert_eq!(view.ifd().fields().len(), want, "{dialect:?}");
        for field in view.ifd().fields() {
            if dialect == MakerNoteDialect::Nikon3 && RECORDS.contains(&field.tag) {
                continue;
            }
            let ours = match &field.value {
                Value::Byte(values) | Value::Undefined(values) => joined(values),
                Value::Short(values) => joined(values),
                Value::SShort(values) => joined(values),
                Value::Long(values) => joined(values),
                Value::Rational(values) => ratio(values),
                Value::SRational(values) => joined(
                    &values
                        .iter()
                        .map(|(n, d)| format!("{n}/{d}"))
                        .collect::<Vec<_>>(),
                ),
                Value::Ascii(text) => text.split('\0').next().unwrap_or_default().to_owned(),
                other => panic!("unexpected value {other:?}"),
            };
            let key = format!("Exif.{dialect:?}.0x{:04x}", field.tag);
            assert_eq!(exiv2(&stream, &key), ours, "{key} {order:?}");
        }
    }
}

/// Reading through the whole stream, as exiv2 does, yields the view the note's bytes alone give.
#[test]
fn the_stream_and_the_note_alone_give_the_same_view() {
    for (dialect, order, stream, note_at) in streams() {
        let exif = Exif::parse(&stream).expect("parse");
        let layout = exif.maker_note_layout().expect("a layout");
        assert_eq!(layout.dialect, dialect);
        assert_eq!(layout.byte_order, order, "{dialect:?}");
        let read = NikonMakerNote::read(&stream[..], note_at, layout).expect("reads");
        assert_eq!(read, Some(view(&stream)), "{dialect:?} {order:?}");
    }
}

/// Building the view changes nothing: the unmodified model re-emits the stream byte for byte,
/// the records' enciphered bytes included.
#[test]
fn an_unmodified_nikon_stream_round_trips_byte_identically() {
    for (dialect, order, stream, _) in streams() {
        let exif = Exif::parse(&stream).expect("parse");
        let _ = view(&stream);
        let rewritten = ExifWriter::new()
            .with_marker(false)
            .write(&exif)
            .expect("write");
        assert_eq!(rewritten, stream, "{dialect:?} {order:?}");
    }
}
