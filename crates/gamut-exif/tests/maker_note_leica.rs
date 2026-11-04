//! The Leica `MakerNote` dialects and view against ExifTool 13.59 (issue #978), over the two notes
//! the sample corpus holds, extracted byte for byte into `tests/fixtures/`.
//!
//! exiv2 has no Leica module, so the oracle is ExifTool's reading of the two source files,
//! committed in `references/exif/leica/` and transcribed, raw as stored, into the table there; each
//! expected value below is that table's "raw" column.

use gamut_exif::{
    ByteOrder, Exif, ExifTag, ExifWriter, LeicaMakerNote, MakerNote, MakerNoteDialect,
    MakerNoteVendor, OffsetBase, Rational, SRational, Value,
};

/// `leica/m10/f5381888.dng`'s note, which stood at TIFF offset 6144.
const M10: &[u8] = include_bytes!("fixtures/leica_m10_maker_note.bin");
const M10_AT: u64 = 6144;
/// `leica/m-monochrom/L1000622.DNG`'s note, which stood at TIFF offset 1152.
const M_MONOCHROM: &[u8] = include_bytes!("fixtures/leica_m_monochrom_maker_note.bin");
const M_MONOCHROM_AT: u64 = 1152;

/// Both corpus files are little-endian TIFF streams.
const ORDER: ByteOrder = ByteOrder::LittleEndian;

fn leica(bytes: &[u8]) -> MakerNote {
    MakerNote {
        // Both files name `Leica Camera AG` in `Make`.
        vendor: MakerNoteVendor::detect(Some("Leica Camera AG")),
        bytes: bytes.to_vec(),
    }
}

fn view(bytes: &[u8], at: u64) -> LeicaMakerNote {
    leica(bytes)
        .leica(ORDER, at)
        .expect("reads")
        .expect("a Leica note")
}

#[test]
fn the_m10_note_is_leica9_with_tiff_header_offsets() {
    let layout = leica(M10).layout(ORDER).expect("a layout");
    assert_eq!(layout.dialect, MakerNoteDialect::Leica9);
    assert_eq!(layout.byte_order, ORDER);
    assert_eq!(layout.ifd_offset, 8);
    assert_eq!(layout.offset_base, OffsetBase::TiffHeader);
}

#[test]
fn the_m_monochrom_note_is_leica4_with_note_relative_offsets() {
    let layout = leica(M_MONOCHROM).layout(ORDER).expect("a layout");
    assert_eq!(layout.dialect, MakerNoteDialect::Leica4);
    assert_eq!(layout.byte_order, ORDER);
    assert_eq!(layout.ifd_offset, 8);
    assert_eq!(layout.offset_base, OffsetBase::MakerNote { at: 0 });
}

#[test]
fn the_m10_tags_agree_with_exiftool() {
    let view = view(M10, M10_AT);
    assert!(view.unreadable_tags().is_empty());
    assert_eq!(view.ifd().fields().len(), 25);
    let tags = view.leica9().expect("Leica9");
    assert_eq!(tags.focus_distance(), Some(16_279));
    let zero = Some(SRational { num: 0, den: 1 });
    assert_eq!(tags.external_sensor_brightness_value(), zero);
    assert_eq!(tags.measured_lv(), zero);
    assert_eq!(tags.user_profile(), Some(""));
    assert_eq!(
        view.ifd().get(0x034c),
        Some(&Value::Ascii("\0\0\0\0".into()))
    );
    assert_eq!(tags.iso_selected(), Some(100));
    assert_eq!(tags.f_number(), Some(5_600));
    assert_eq!(tags.correlated_color_temp(), Some(5_200));
    assert_eq!(tags.color_tint(), Some(6));
    assert_eq!(
        tags.white_point(),
        Some(&[(3_399, 10_000), (3_509, 10_000)][..])
    );
    assert_eq!(tags.lens_profile_name(), None);
}

/// Read from the note's own position, every out-of-line `Leica9` value lies in the note; read as
/// if the note stood anywhere else, they do not.
#[test]
fn the_m10_offsets_count_from_the_tiff_header() {
    let moved = view(M10, 0);
    assert_eq!(
        moved.unreadable_tags(),
        &[0x0311, 0x0312, 0x034c, 0x035d, 0x035e, 0x035f]
    );
    // An inline value reads wherever the note stands.
    assert_eq!(
        moved.leica9().expect("Leica9").focus_distance(),
        Some(16_279)
    );
}

#[test]
fn the_m_monochrom_tags_agree_with_exiftool() {
    let view = view(M_MONOCHROM, M_MONOCHROM_AT);
    assert!(view.unreadable_tags().is_empty());
    assert_eq!(view.ifd().fields().len(), 4);
    let sizes = [(0x3000, 20), (0x3100, 14), (0x3400, 11), (0x3900, 4)];
    for (tag, entries) in sizes {
        let sub = view.subdirectory(tag).expect("a subdirectory");
        assert_eq!(sub.fields().len(), entries, "{tag:#06x}");
    }
    let tags = view.leica4().expect("Leica4");
    assert_eq!(tags.contrast(), Some(2));
    assert_eq!(tags.sharpening(), Some(2));
    assert_eq!(tags.saturation(), Some(2));
    assert_eq!(tags.white_balance(), Some(0));
    assert_eq!(tags.jpeg_quality(), Some(97));
    assert_eq!(tags.wb_rgb_levels(), Some(&[(16_384, 16_384); 3][..]));
    assert_eq!(tags.user_profile(), Some(""));
    assert_eq!(tags.jpeg_size(), Some(0));
    assert_eq!(tags.serial_number(), Some("4348886"));
    assert_eq!(tags.firmware_version(), Some("1.008"));
    assert_eq!(tags.base_iso(), Some(320));
    assert_eq!(tags.sensor_width(), Some(5_270));
    assert_eq!(tags.sensor_height(), Some(3_516));
    assert_eq!(tags.sensor_bit_depth(), Some(14));
    assert_eq!(tags.camera_temperature(), Some(11));
    assert_eq!(tags.lens_type(), Some(135));
    assert_eq!(
        tags.approximate_f_number(),
        Some(Rational {
            num: 425_984,
            den: 65_536
        })
    );
    assert_eq!(tags.measured_lv(), Some(264_448));
    assert_eq!(tags.external_sensor_brightness_value(), Some(378_880));
}

/// `Leica4` offsets count from the note's start, so the note reads the same at any position.
#[test]
fn the_m_monochrom_note_reads_the_same_anywhere() {
    assert_eq!(view(M_MONOCHROM, 0), view(M_MONOCHROM, M_MONOCHROM_AT));
}

/// A bare TIFF stream whose Exif sub-IFD carries `note` under a Leica `Make`.
fn leica_stream(note: &[u8]) -> Vec<u8> {
    let mut exif = Exif::new(ORDER);
    exif.set_tag(ExifTag::Make, Value::Ascii("Leica Camera AG".into()));
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(note.to_vec()));
    ExifWriter::new().marker(false).write(&exif).expect("write")
}

/// The model's own path resolves the vendor from `Make`, and a note-relative view reads whole
/// wherever the writer put the note.
#[test]
fn a_parsed_model_reads_the_m_monochrom_note() {
    let stream = leica_stream(M_MONOCHROM);
    let exif = Exif::parse(&stream).expect("parse");
    let note = exif.maker_note().expect("a note");
    assert_eq!(note.vendor, MakerNoteVendor::Leica);
    let at = exif.maker_note_offset().expect("an offset");
    let view = note
        .leica(exif.byte_order(), at)
        .expect("reads")
        .expect("Leica");
    assert_eq!(view, self::view(M_MONOCHROM, M_MONOCHROM_AT));
    let layout = exif.maker_note_layout().expect("a layout");
    let read = LeicaMakerNote::read(&stream[..], at, layout).expect("reads");
    assert_eq!(read, Some(view));
}

/// Building the view changes nothing: each unmodified model re-emits its stream byte for byte.
#[test]
fn an_unmodified_leica_stream_round_trips_byte_identically() {
    for note in [M10, M_MONOCHROM] {
        let stream = leica_stream(note);
        let exif = Exif::parse(&stream).expect("parse");
        let maker = exif.maker_note().expect("a note");
        let at = exif.maker_note_offset().expect("an offset");
        let _ = maker.leica(exif.byte_order(), at).expect("reads");
        assert_eq!(maker.bytes, note);
        let rewritten = ExifWriter::new().marker(false).write(&exif).expect("write");
        assert_eq!(rewritten, stream);
    }
}
