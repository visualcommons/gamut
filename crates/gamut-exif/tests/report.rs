//! The honesty contract for the lenient reader: nothing is discarded without being named.
//!
//! [`ExifReader`] drops a malformed sub-IFD or an out-of-bounds thumbnail range so the rest of a
//! real-world blob still parses. That leniency is only defensible if a caller can *ask* what it
//! cost — otherwise a blob that never carried GPS and one whose GPS pointer was dangling are
//! indistinguishable. Each test below feeds one deliberately broken blob to
//! [`ExifReader::parse_with_report`] and pins that the discarded region is named with the tag that
//! addressed it, the offset it carried, and a reason that separates "nothing could be there" from
//! "something was there and it was corrupt". Three further tests pin the contract's edges: that a
//! *strict* report is not empty of the one loss strictness has no grounds to reject, that the
//! report's offsets and the crate's error offsets are deliberately in different frames, and — over
//! a truncation sweep — that no sub-IFD is ever dropped without being named.

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
/// The `Exif\0\0` marker a JPEG `APP1` payload carries before the TIFF stream.
const MARKER: &[u8] = b"Exif\x00\x00";

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
        assert_eq!(
            dropped.tag(),
            Some(tag),
            "named by the tag that addressed it"
        );
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
    assert_eq!(dropped.tag(), Some(THUMB_OFFSET));
    assert_eq!(dropped.offset(), u64::from(DANGLING));
    assert_eq!(dropped.reason(), DropReason::OutOfBounds);
}

/// A blob that parses in full reports nothing.
///
/// `is_empty` is the verdict over the regions the report *covers* — deliberately not "this parse
/// lost nothing", which is a stronger claim this crate cannot make (see the `report` module). What
/// it must still guarantee is the direction tested here: a healthy file names nothing, or the
/// signal would be noise.
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

/// A top-level directory past the 1st IFD is named rather than silently discarded.
///
/// EXIF defines exactly two — the 0th (primary image) and the 1st (thumbnail) — so a longer
/// next-IFD chain parses cleanly and then has nowhere to go in the model. That is a real loss (the
/// bytes do not survive `to_bytes`), and it is the one drop with no addressing tag: the chain is
/// followed through the structural next-IFD pointer, so the reported tag is `None` and the reported
/// offset is the directory's own position.
#[test]
fn a_top_level_directory_past_the_thumbnail_is_named() {
    for extra in 1..=2 {
        let mut thumb = Ifd::new();
        thumb.set(0x0103, Value::Short(vec![6])); // Compression = JPEG

        let mut ifds = vec![image_ifd(), thumb];
        for n in 0..extra {
            let mut trailing = Ifd::new();
            trailing.set(0x0131, Value::Ascii(format!("trailing {n}"))); // Software
            ifds.push(trailing);
        }
        let bytes = tiff(ifds);

        let (exif, report) = ExifReader::new()
            .parse_with_report(&bytes)
            .expect("a long chain must still parse");
        assert_eq!(exif.make(), Some("Canon"), "the 0th IFD survives");
        assert!(exif.thumbnail().is_some(), "the 1st IFD survives");

        // Where those directories actually sit, read back independently of the model.
        let mut raw = IfdReader::open(&bytes[..]).expect("open");
        let offsets: Vec<u64> = raw
            .ifds()
            .map(|ifd| ifd.expect("chain link").offset)
            .collect();
        assert_eq!(
            offsets.len(),
            2 + extra,
            "the fixture really has a long chain"
        );

        assert_eq!(
            report.dropped().len(),
            extra,
            "every trailing directory is named: {:?}",
            report.dropped()
        );
        for (dropped, expected) in report.dropped().iter().zip(&offsets[2..]) {
            assert_eq!(dropped.region(), DroppedRegion::TrailingIfd);
            assert_eq!(
                dropped.tag(),
                None,
                "no tag addresses a top-level directory"
            );
            assert_eq!(dropped.offset(), *expected, "named at its own position");
            assert_eq!(dropped.reason(), DropReason::Unrepresentable);
        }
    }
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
            let named = report.dropped().iter().any(|d| d.tag() == Some(tag));
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

/// A thumbnail offset with no length beside it is named rather than silently ignored.
///
/// An offset with no `JPEGInterchangeFormatLength` is not "no thumbnail" — it is an address with
/// nothing to size the read by, and the JPEG behind it is lost. Before this the pair fell into the
/// reader's catch-all `None` arm: no bytes, no error, no report entry, inside the very region this
/// report claims completeness over. The rule is structural, not a support level: Exif 3.0 §4.6.9.2
/// Table 21 states the pair's level per thumbnail-format column, an axis that is not the two-valued
/// `Compression` tag — which this reader does not consult at all: `Thumbnail::compression` exposes
/// it, but nothing in the parse branches on it (issue #574) — so the fixture's `Compression` value
/// is scene-setting, not the trigger.
#[test]
fn a_thumbnail_offset_without_a_length_is_named() {
    let mut thumb = Ifd::new();
    thumb.set(0x0103, Value::Short(vec![6])); // Compression = JPEG
    thumb.set(THUMB_OFFSET, Value::Long(vec![4])); // ...in bounds, so not OutOfBounds
    // ...and deliberately no THUMB_LENGTH.

    let (exif, report) = ExifReader::new()
        .parse_with_report(&tiff(vec![image_ifd(), thumb]))
        .expect("lenient parse");

    assert_eq!(
        exif.thumbnail().and_then(|t| t.jpeg()),
        None,
        "there is no length, so there are no bytes"
    );
    assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
    let dropped = report.dropped()[0];
    assert_eq!(dropped.region(), DroppedRegion::ThumbnailJpeg);
    assert_eq!(dropped.tag(), Some(THUMB_OFFSET));
    assert_eq!(dropped.offset(), 4, "named at the offset the tag carried");
    assert_eq!(
        dropped.reason(),
        DropReason::ThumbnailLengthMissing,
        "not OutOfBounds — the address is inside the blob; the length is what is missing"
    );
}

/// A thumbnail with neither JPEG tag is an uncompressed thumbnail, not a loss, and reports nothing.
///
/// The other direction of the pair: a `JPEGInterchangeFormatLength` on its own addresses no bytes
/// at all, so there is nothing to name. Without this, reporting the incomplete pair could be
/// "fixed" by reporting every thumbnail that has no JPEG, which would make the signal noise.
///
/// This pins the *reporting* contract only. Whether a length-only 1st IFD should nonetheless be
/// *rejected* in strict mode is open, and filed as issue #574: it is equally non-conformant (Exif
/// 3.0 §4.6.9.2 Table 21 gives both tags one level per thumbnail-format column — `M` under
/// **Compressed**, `N` under the three uncompressed ones — and that axis is not the two-valued
/// `Compression` tag), but it is not equally a *loss*, which is what this report names. An offset
/// with no length addresses bytes; a length with no offset addresses nothing. Both fixtures here
/// are uncompressed, where the table forbids either tag.
#[test]
fn a_thumbnail_with_no_jpeg_range_reports_nothing() {
    for extra in [None, Some((THUMB_LENGTH, 16))] {
        let mut thumb = Ifd::new();
        thumb.set(0x0103, Value::Short(vec![1])); // Compression = uncompressed
        if let Some((tag, value)) = extra {
            thumb.set(tag, Value::Long(vec![value]));
        }
        let (exif, report) = ExifReader::new()
            .parse_with_report(&tiff(vec![image_ifd(), thumb]))
            .expect("lenient parse");
        assert!(
            exif.thumbnail().is_some(),
            "the 1st IFD is still a thumbnail"
        );
        assert!(
            report.is_empty(),
            "nothing was addressed, so nothing was dropped: {:?}",
            report.dropped()
        );
    }
}

/// A strict report is not always empty: it still carries the loss strictness cannot reject.
///
/// Strictness rejects *malformed* regions. A top-level directory past the 1st IFD is not malformed
/// — it parses cleanly and the [`Exif`](gamut_exif::Exif) model simply has nowhere to put it — so
/// strict has no grounds to fail on it, and dropping it silently would re-hide exactly the loss
/// `DroppedRegion::TrailingIfd` exists to surface.
#[test]
fn a_strict_parse_still_reports_a_trailing_directory() {
    let mut thumb = Ifd::new();
    thumb.set(0x0103, Value::Short(vec![6])); // Compression = JPEG
    let mut trailing = Ifd::new();
    trailing.set(0x0131, Value::Ascii("trailing".into())); // Software

    let (exif, report) = ExifReader::new()
        .strict(true)
        .parse_with_report(&tiff(vec![image_ifd(), thumb, trailing]))
        .expect("a well-formed long chain must not fail even in strict mode");

    assert_eq!(exif.make(), Some("Canon"), "the 0th IFD survives");
    assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
    assert_eq!(report.dropped()[0].region(), DroppedRegion::TrailingIfd);
    assert_eq!(report.dropped()[0].reason(), DropReason::Unrepresentable);
}

/// The report's offsets and the crate's error offsets are in different frames, by the marker.
///
/// A `Dropped::offset` addresses the TIFF stream, so it matches every offset stored inside the file
/// and is unchanged by whether the caller's buffer carries the six-byte `Exif\0\0` marker. An error
/// message instead names a byte of the buffer that was handed in, so the marker shifts it. Pinning
/// the pair together is what stops either frame drifting onto the other: unifying them would aim a
/// diagnostic outside the caller's buffer or renumber every reported offset.
#[test]
fn report_offsets_ignore_the_marker_but_error_offsets_include_it() {
    let mut image = image_ifd();
    image.set(GPS_INFO, Value::Long(vec![DANGLING]));
    let bare = tiff(vec![image]);
    let mut marked = MARKER.to_vec();
    marked.extend(&bare);

    // The report frame is marker-invariant.
    let offsets = |blob: &[u8]| -> Vec<u64> {
        let (_, report) = ExifReader::new().parse_with_report(blob).expect("parse");
        report.dropped().iter().map(|d| d.offset()).collect()
    };
    assert_eq!(offsets(&bare), vec![u64::from(DANGLING)]);
    assert_eq!(
        offsets(&marked),
        offsets(&bare),
        "a report offset addresses the TIFF stream, not the caller's buffer"
    );

    // The diagnostic frame is marker-shifted. The same corruption is applied to the TIFF stream in
    // both blobs, so any offset difference is the marker and nothing else.
    let mut broken_bare = bare.clone();
    broken_bare[4] ^= 0xFF; // the first-IFD offset in the TIFF header
    let mut broken_marked = MARKER.to_vec();
    broken_marked.extend(&broken_bare);

    let message = |blob: &[u8]| -> String {
        ExifReader::new()
            .parse(blob)
            .expect_err("a corrupt first-IFD offset must fail")
            .to_string()
    };
    let (bare_msg, marked_msg) = (message(&broken_bare), message(&broken_marked));
    let at = |m: &str| -> u64 {
        let tail = m
            .rsplit_once("byte offset: ")
            .expect("the diagnostic names a byte offset")
            .1;
        tail.trim_end_matches(']')
            .parse()
            .expect("the byte offset parses")
    };
    assert_eq!(
        at(&marked_msg) - at(&bare_msg),
        MARKER.len() as u64,
        "an error offset counts the marker: {marked_msg} vs {bare_msg}"
    );
}
