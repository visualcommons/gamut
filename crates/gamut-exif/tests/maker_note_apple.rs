//! The Apple `MakerNote` view against ExifTool 13.59 (issue #976), over the one note the sample
//! corpus holds, extracted byte for byte into `tests/fixtures/`.
//!
//! exiv2 has no Apple module, so the oracle is ExifTool's reading of the source file, committed in
//! `references/exif/apple/` and transcribed, raw as stored, into the table there; each expected
//! value below is that table's "Fixture: raw" column.

use gamut_exif::{
    AppleMakerNote, ByteOrder, Exif, ExifTag, ExifWriter, MakerNote, MakerNoteDialect,
    MakerNoteVendor, OffsetBase, SRational, Value,
};

/// `apple/iphone-12-pro/IMG_1361.DNG`'s note, which stood at TIFF offset 3276.
const IPHONE_12_PRO: &[u8] = include_bytes!("fixtures/apple_iphone_12_pro_maker_note.bin");
const IPHONE_12_PRO_AT: u64 = 3276;

/// The corpus file is a big-endian TIFF stream, and so is the note's IFD (its mark is `MM`).
const ORDER: ByteOrder = ByteOrder::BigEndian;

fn apple(bytes: &[u8]) -> MakerNote {
    MakerNote {
        vendor: MakerNoteVendor::detect(Some("Apple")),
        bytes: bytes.to_vec(),
    }
}

fn view() -> AppleMakerNote {
    apple(IPHONE_12_PRO)
        .apple(ORDER, IPHONE_12_PRO_AT)
        .expect("reads")
        .expect("an Apple note")
}

#[test]
fn the_iphone_note_is_apple_with_note_relative_offsets() {
    let layout = apple(IPHONE_12_PRO).layout(ORDER).expect("a layout");
    assert_eq!(layout.dialect, MakerNoteDialect::Apple);
    assert_eq!(layout.byte_order, ORDER);
    assert_eq!(layout.ifd_offset, 14);
    assert_eq!(layout.offset_base, OffsetBase::MakerNote { at: 0 });
}

#[test]
fn the_iphone_tags_agree_with_exiftool() {
    let view = view();
    assert!(view.unreadable_tags().is_empty());
    // ExifTool's "MakerNotes directory with 36 entries".
    assert_eq!(view.ifd().fields().len(), 36);
    assert_eq!(view.maker_note_version(), Some(12));
    let ae_matrix = view.ae_matrix().expect("AEMatrix");
    assert_eq!((ae_matrix.len(), &ae_matrix[..8]), (558, &b"bplist00"[..]));
    let run_time = view.run_time().expect("RunTime");
    assert_eq!((run_time.len(), &run_time[..8]), (104, &b"bplist00"[..]));
    assert_eq!(view.ae_stable(), Some(1));
    assert_eq!(view.ae_target(), Some(186));
    assert_eq!(view.ae_average(), Some(180));
    assert_eq!(view.af_stable(), Some(1));
    assert_eq!(
        view.acceleration_vector(),
        Some(&[(-1_702, 415_935), (-6_172, 6_145), (-4_786, 445_081)][..])
    );
    assert_eq!(
        view.focus_distance_range(),
        Some(&[(67, 256), (97, 256)][..])
    );
    assert_eq!(view.image_capture_type(), Some(10));
    assert_eq!(view.live_photo_video_index(), Some(1_112_547_584));
    assert_eq!(view.image_processing_flags(), Some(65_536));
    assert_eq!(view.quality_hint(), Some("q900n"));
    assert_eq!(view.photos_app_feature_flags(), Some(0));
    assert_eq!(
        view.image_capture_request_id(),
        Some("6D45E6A1-BE56-43A0-B1A8-0175E380A06A")
    );
    assert_eq!(
        view.hdr_headroom(),
        Some(SRational {
            num: 63_809,
            den: 37_837
        })
    );
    assert_eq!(view.af_performance(), Some(&[954, 268_435_495][..]));
    assert_eq!(view.scene_flags(), Some(134));
    assert_eq!(view.signal_to_noise_ratio_type(), Some(3));
    assert_eq!(
        view.signal_to_noise_ratio(),
        Some(SRational {
            num: 466_861,
            den: 7_493
        })
    );
    assert_eq!(
        view.photo_identifier(),
        Some("C465536F-F5F5-4458-A135-EF72CDEA1BE5")
    );
    assert_eq!(view.color_temperature(), Some(5_358));
    assert_eq!(view.camera_type(), Some(1));
    assert_eq!(view.focus_position(), Some(55));
    assert_eq!(view.hdr_gain(), Some(SRational { num: 0, den: 1 }));
    assert_eq!(view.af_measured_depth(), Some(403));
    assert_eq!(view.af_confidence(), Some(0));
}

/// The tags the fixture does not hold read as absent, and the ones `Apple.pm` leaves unnamed stay
/// reachable by number (`Apple_0x0036` is 65535 in ExifTool's reading).
#[test]
fn the_iphone_note_keeps_its_untyped_tags() {
    let view = view();
    assert_eq!(view.hdr_image_type(), None);
    assert_eq!(view.burst_uuid(), None);
    assert_eq!(view.ois_mode(), None);
    assert_eq!(view.content_identifier(), None);
    assert_eq!(view.image_unique_id(), None);
    assert_eq!(view.luminance_noise_amplitude(), None);
    assert_eq!(view.green_ghost_mitigation_status(), None);
    assert_eq!(view.ifd().get(0x0036), Some(&Value::SLong(vec![65_535])));
}

/// A bare TIFF stream whose Exif sub-IFD carries `note` under `Make` `Apple`, in `order`.
fn apple_stream(note: &[u8], order: ByteOrder) -> Vec<u8> {
    let mut exif = Exif::new(order);
    exif.set_tag(ExifTag::Make, Value::Ascii("Apple".into()));
    exif.set_tag(ExifTag::MakerNote, Value::Undefined(note.to_vec()));
    ExifWriter::new()
        .with_marker(false)
        .write(&exif)
        .expect("write")
}

/// The model's own path resolves the vendor from `Make`, and the note-relative view reads whole
/// wherever the writer put the note — in an outer stream of either byte order, since the note's
/// own mark decides its IFD's.
#[test]
fn a_parsed_model_reads_the_iphone_note() {
    for order in [ByteOrder::BigEndian, ByteOrder::LittleEndian] {
        let stream = apple_stream(IPHONE_12_PRO, order);
        let exif = Exif::parse(&stream).expect("parse");
        let note = exif.maker_note().expect("a note");
        assert_eq!(note.vendor, MakerNoteVendor::Apple);
        let at = exif.maker_note_offset().expect("an offset");
        assert_ne!(at, IPHONE_12_PRO_AT);
        let view = note
            .apple(exif.byte_order(), at)
            .expect("reads")
            .expect("Apple");
        assert_eq!(view, self::view());
        let layout = exif.maker_note_layout().expect("a layout");
        let read = AppleMakerNote::read(&stream[..], at, layout).expect("reads");
        assert_eq!(read, Some(view));
    }
}

/// Building the view changes nothing: the unmodified model re-emits its stream byte for byte.
#[test]
fn an_unmodified_apple_stream_round_trips_byte_identically() {
    let stream = apple_stream(IPHONE_12_PRO, ORDER);
    let exif = Exif::parse(&stream).expect("parse");
    let maker = exif.maker_note().expect("a note");
    let at = exif.maker_note_offset().expect("an offset");
    let _ = maker.apple(exif.byte_order(), at).expect("reads");
    assert_eq!(maker.bytes, IPHONE_12_PRO);
    let rewritten = ExifWriter::new()
        .with_marker(false)
        .write(&exif)
        .expect("write");
    assert_eq!(rewritten, stream);
}
