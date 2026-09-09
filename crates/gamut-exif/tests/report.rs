//! The honesty contract for the lenient reader: nothing is discarded without being named.
//!
//! [`ExifReader`] drops a malformed sub-IFD or an out-of-bounds thumbnail range so the rest of a
//! real-world blob still parses. That leniency is only defensible if a caller can *ask* what it
//! cost — otherwise a blob that never carried GPS and one whose GPS pointer was dangling are
//! indistinguishable. Each test below feeds one deliberately broken blob to
//! [`ExifReader::parse_with_report`] and pins that the discarded region is named with the tag that
//! addressed it, the offset it carried, and a reason that separates "nothing could be there" from
//! "something was there and it was corrupt". The last test generalises it over a truncation sweep.

use gamut_exif::{DropReason, DroppedRegion, ExifReader};
use gamut_ifd::{ByteOrder, Ifd, IfdReader, TiffFile, Value, Variant, write};

/// `ExifIFD` pointer (Exif 3.0 §4.6.3).
const EXIF_IFD: u16 = 0x8769;
/// `GPSInfo` pointer.
const GPS_INFO: u16 = 0x8825;
/// `Interoperability` pointer, which lives inside the Exif sub-IFD.
const INTEROP_IFD: u16 = 0xA005;
/// `JPEGInterchangeFormat` — the 1st IFD's thumbnail offset.
const THUMB_OFFSET: u16 = 0x0201;
/// `JPEGInterchangeFormatLength`.
const THUMB_LENGTH: u16 = 0x0202;
/// An offset far past the end of any fixture here.
const DANGLING: u32 = 0xFFFF;

/// Serialises `ifds` as a bare little-endian TIFF stream.
fn tiff(ifds: Vec<Ifd>) -> Vec<u8> {
    write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds,
    })
    .expect("write")
}

/// A 0th IFD carrying `Make`, so every fixture has one tag that must survive the drop.
fn image_ifd() -> Ifd {
    let mut image = Ifd::new();
    image.set(0x010F, Value::Ascii("Canon".into()));
    image
}

/// A dangling Exif, GPS or Interop pointer is reported with the tag that addressed it, the offset
/// it carried, and `OutOfBounds` — while the rest of the blob still parses.
#[test]
fn a_dangling_sub_ifd_pointer_is_named_with_its_tag_offset_and_reason() {
    for (tag, region) in [
        (EXIF_IFD, DroppedRegion::ExifIfd),
        (GPS_INFO, DroppedRegion::GpsIfd),
        (INTEROP_IFD, DroppedRegion::InteropIfd),
    ] {
        // The Interop pointer is reached from inside the Exif sub-IFD, not the 0th IFD.
        let mut image = image_ifd();
        if tag == INTEROP_IFD {
            let mut exif = Ifd::new();
            exif.set(INTEROP_IFD, Value::Long(vec![DANGLING]));
            image.set_sub_ifd(EXIF_IFD, vec![exif]);
        } else {
            image.set(tag, Value::Long(vec![DANGLING]));
        }

        let (exif, report) = ExifReader::new()
            .parse_with_report(&tiff(vec![image]))
            .expect("a dangling pointer must not fail a lenient parse");

        assert_eq!(exif.make(), Some("Canon"), "the rest of the blob survives");
        assert_eq!(
            report.dropped().len(),
            1,
            "exactly one region was dropped for {region:?}: {:?}",
            report.dropped()
        );
        let dropped = report.dropped()[0];
        assert_eq!(dropped.region(), region);
        assert_eq!(dropped.tag(), tag, "named by the tag that addressed it");
        assert_eq!(dropped.offset(), u64::from(DANGLING));
        assert_eq!(dropped.reason(), DropReason::OutOfBounds);
    }
}

/// A pointer that lands *inside* the blob but on bytes that are not a directory is `Malformed`,
/// not `OutOfBounds` — the two are different defects, and a report that conflated them would send
/// a caller looking in the wrong place.
#[test]
fn an_in_bounds_pointer_to_corrupt_bytes_is_reported_as_malformed() {
    let mut image = image_ifd();
    // Offset 1 is inside the stream but straddles the byte-order mark and the magic, so the entry
    // count read there is nonsense (0x2A49 entries) and the directory cannot be read.
    image.set(EXIF_IFD, Value::Long(vec![1]));
    let bytes = tiff(vec![image]);
    assert!(bytes.len() > 1, "offset 1 must really be inside the stream");

    let (exif, report) = ExifReader::new()
        .parse_with_report(&bytes)
        .expect("lenient parse");

    assert!(exif.exif_ifd().is_none(), "the sub-IFD was dropped");
    assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
    assert_eq!(report.dropped()[0].reason(), DropReason::Malformed);
    assert_eq!(report.dropped()[0].offset(), 1);
}

/// A thumbnail whose JPEG range runs past the end of the blob loses its bytes, and the loss is
/// named — the thumbnail's own directory survives, so without the report the missing bytes look
/// like a thumbnail that never had any.
#[test]
fn an_out_of_bounds_thumbnail_range_is_named() {
    let mut thumb = Ifd::new();
    thumb.set(THUMB_OFFSET, Value::Long(vec![DANGLING]));
    thumb.set(THUMB_LENGTH, Value::Long(vec![16]));

    let (exif, report) = ExifReader::new()
        .parse_with_report(&tiff(vec![image_ifd(), thumb]))
        .expect("lenient parse");

    let thumbnail = exif.thumbnail().expect("the 1st IFD is still a thumbnail");
    assert_eq!(
        thumbnail.jpeg(),
        None,
        "no bytes, rather than bytes from nowhere"
    );
    assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
    let dropped = report.dropped()[0];
    assert_eq!(dropped.region(), DroppedRegion::ThumbnailJpeg);
    assert_eq!(dropped.tag(), THUMB_OFFSET);
    assert_eq!(dropped.offset(), u64::from(DANGLING));
    assert_eq!(dropped.reason(), DropReason::OutOfBounds);
}

/// A blob that parses in full reports nothing: `is_empty` is the "this parse lost nothing" verdict,
/// so a report that named a region on a healthy file would make it useless.
#[test]
fn a_well_formed_blob_reports_no_drops() {
    let mut interop = Ifd::new();
    interop.set(0x0001, Value::Ascii("R98".into()));
    let mut exif = Ifd::new();
    exif.set(0x829D, Value::Rational(vec![(28, 10)]));
    exif.set_sub_ifd(INTEROP_IFD, vec![interop]);
    let mut gps = Ifd::new();
    gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0]));

    let mut image = image_ifd();
    image.set_sub_ifd(EXIF_IFD, vec![exif]);
    image.set_sub_ifd(GPS_INFO, vec![gps]);

    let (parsed, report) = ExifReader::new()
        .parse_with_report(&tiff(vec![image]))
        .expect("parse");

    assert!(parsed.exif_ifd().is_some());
    assert!(parsed.gps_ifd().is_some());
    assert!(parsed.interop_ifd().is_some());
    assert!(
        report.is_empty(),
        "healthy blob reported {:?}",
        report.dropped()
    );
}

/// The law, over a truncation sweep: whenever a lenient parse succeeds, a sub-IFD pointer that was
/// present in the source has either been followed into the model or been named in the report —
/// never silently missing.
///
/// Truncation is the cheapest generator of *varied* corruption: each prefix breaks a different
/// structure (a value, a directory body, a pointer target), so the sweep reaches drop paths no
/// hand-written fixture enumerates.
#[test]
fn a_truncated_blob_never_drops_a_sub_ifd_without_naming_it() {
    let mut interop = Ifd::new();
    interop.set(0x0001, Value::Ascii("R98".into()));
    let mut exif = Ifd::new();
    exif.set(0x829D, Value::Rational(vec![(28, 10)]));
    exif.set_sub_ifd(INTEROP_IFD, vec![interop]);
    let mut gps = Ifd::new();
    gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0]));
    let mut image = image_ifd();
    image.set_sub_ifd(EXIF_IFD, vec![exif]);
    image.set_sub_ifd(GPS_INFO, vec![gps]);
    let full = tiff(vec![image]);

    let mut parses = 0_usize;
    let mut drops = 0_usize;
    for end in 0..full.len() {
        let data = &full[..end];
        let Ok((parsed, report)) = ExifReader::new().parse_with_report(data) else {
            continue;
        };
        parses += 1;

        // What the source actually carried, read back independently of the model.
        let Ok(mut raw_reader) = IfdReader::open(data) else {
            continue;
        };
        let first = raw_reader.first_ifd_offset();
        let Ok(raw) = raw_reader.read_ifd(first) else {
            continue;
        };

        for (tag, followed) in [
            (EXIF_IFD, parsed.exif_ifd().is_some()),
            (GPS_INFO, parsed.gps_ifd().is_some()),
        ] {
            if raw.entry(tag).is_none() {
                continue;
            }
            let named = report.dropped().iter().any(|d| d.tag() == tag);
            assert_ne!(
                followed, named,
                "at truncation {end}, tag {tag:#06x} was followed={followed} and named={named}"
            );
            drops += usize::from(named);
        }
    }

    assert!(
        parses > 0,
        "no truncation parsed — the sweep proved nothing"
    );
    assert!(
        drops > 0,
        "no truncation dropped a sub-IFD — the sweep proved nothing"
    );
}
