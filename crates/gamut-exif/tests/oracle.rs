//! Differential conformance against exiv2's EXIF parser/serializer, the engine the crate documents
//! as its oracle. These cross-checks complement the spec-derived golden vectors: the golden tests
//! pin exact bytes, while these prove gamut interoperates with the reference implementation — its
//! output is real EXIF that exiv2 reads, and it reads exiv2's output back. Equality is asserted on
//! re-parsed tag *values*, not on exiv2's bytes (exiv2 lays out and orders the stream its own way).
//!
//! Requires the `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use exiv2_oracle::{exif_count, exif_get, exif_roundtrip};
use gamut_exif::{
    ByteOrder, Exif, ExifTag, ExifWriter, FieldType, IfdKind, Rational, TagCount, Value,
};
use gamut_ifd::tags::{self, TagBlock, TagSpec};

/// A representative model spanning the 0th IFD and the Exif sub-IFD.
fn sample() -> Exif {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("Canon EOS R5".into()));
    exif.set_tag(ExifTag::Orientation, Value::Short(vec![1]));
    exif.set_tag(ExifTag::ExifVersion, Value::Undefined(b"0300".to_vec()));
    exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
    exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
    exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
    exif.set_tag(
        ExifTag::DateTimeOriginal,
        Value::Ascii("2024:01:01 12:00:00".into()),
    );
    exif
}

/// The bare TIFF stream (no `Exif\0\0` marker) that exiv2's `ExifParser` consumes.
fn bare(exif: &Exif) -> Vec<u8> {
    ExifWriter::new().marker(false).write(exif).expect("write")
}

#[test]
fn exiv2_reads_the_standard_tags_gamut_wrote() {
    let bytes = bare(&sample());

    // exiv2 parses the whole stream — the 0th IFD and the Exif sub-IFD behind the ExifIFD pointer.
    let count = exif_count(&bytes).expect("exiv2 decodes gamut's EXIF");
    assert!(count >= 8, "exiv2 read only {count} tags");

    // Values round-trip exactly through the reference reader.
    let get = |key: &str| exif_get(&bytes, key).expect(key);
    assert_eq!(get("Exif.Image.Make"), "Canon");
    assert_eq!(get("Exif.Image.Model"), "Canon EOS R5");
    assert_eq!(get("Exif.Image.Orientation"), "1");
    assert_eq!(get("Exif.Photo.FNumber"), "28/10");
    assert_eq!(get("Exif.Photo.ExposureTime"), "1/250");
    assert_eq!(get("Exif.Photo.ISOSpeedRatings"), "400");
    assert_eq!(get("Exif.Photo.DateTimeOriginal"), "2024:01:01 12:00:00");
}

#[test]
fn gamut_reads_what_exiv2_writes() {
    let original = sample();
    // exiv2 re-encodes the stream into its own canonical layout...
    let exiv2_bytes = exif_roundtrip(&bare(&original)).expect("exiv2 re-encodes");
    // ...and gamut must read the same values back out of it.
    let parsed = Exif::parse(&exiv2_bytes).expect("gamut parses exiv2's EXIF");

    assert_eq!(parsed.make(), Some("Canon"));
    assert_eq!(parsed.model(), Some("Canon EOS R5"));
    assert_eq!(parsed.orientation(), Some(1));
    assert_eq!(parsed.f_number(), Some(Rational { num: 28, den: 10 }));
    assert_eq!(parsed.exposure_time(), Some(Rational { num: 1, den: 250 }));
    assert_eq!(parsed.iso(), Some(400));
    assert_eq!(parsed.datetime_original(), Some("2024:01:01 12:00:00"));
}

/// The `Exif.<group>.<name>` prefix exiv2 keys a directory's tags under.
fn exiv2_group(ifd: IfdKind) -> &'static str {
    match ifd {
        IfdKind::Exif => "Exif.Photo",
        IfdKind::Gps => "Exif.GPSInfo",
        IfdKind::Interop => "Exif.Iop",
        // The 1st IFD shares the 0th IFD's tag definitions, so no ExifTag is classified there.
        _ => "Exif.Image",
    }
}

/// A value satisfying a field-type list and component count, so exiv2 is asked about a tag
/// written the way its specification says to write it.
fn conforming(field_types: &[FieldType], count: TagCount) -> Value {
    let n = match count {
        TagCount::Exact(n) => n as usize,
        TagCount::OneOf(ns) => ns.first().map_or(1, |&n| n as usize),
        // `Any`, including the tags the specification does not constrain at all.
        _ => 2,
    };
    match field_types.first() {
        // A string's component count includes the terminating NUL, so n - 1 characters.
        Some(FieldType::Ascii) => Value::Ascii("A".repeat(n.saturating_sub(1))),
        Some(FieldType::Utf8) => Value::Utf8("A".repeat(n.saturating_sub(1))),
        Some(FieldType::Short) => Value::Short(vec![1; n]),
        Some(FieldType::Long) => Value::Long(vec![1; n]),
        Some(FieldType::Rational) => Value::Rational(vec![(1, 1); n]),
        Some(FieldType::SRational) => Value::SRational(vec![(1, 1); n]),
        Some(FieldType::Undefined) => Value::Undefined(vec![1; n]),
        Some(FieldType::Float) => Value::Float(vec![1.0; n]),
        Some(FieldType::Double) => Value::Double(vec![1.0; n]),
        // `Byte`, and the tags with no type at all.
        _ => Value::Byte(vec![1; n]),
    }
}

/// A value satisfying the tag's CIPA DC-008 field type and component count.
fn conforming_value(tag: ExifTag) -> Value {
    conforming(tag.field_types(), tag.component_count())
}

/// One stream per directory holding every tag gamut catalogues there, written conformantly.
fn stream_of_every_tag(ifd: IfdKind) -> (Vec<ExifTag>, Vec<u8>) {
    let tags: Vec<ExifTag> = ExifTag::ALL
        .iter()
        .copied()
        .filter(|t| t.ifd() == ifd)
        .collect();
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    for &tag in &tags {
        exif.set_tag(tag, conforming_value(tag));
    }
    let bytes = ExifWriter::new()
        .marker(false)
        .write(&exif)
        .expect("gamut writes every catalogued tag");
    (tags, bytes)
}

/// The tags whose canonical CIPA DC-008 name (which gamut uses) is *not* the name exiv2 knows them
/// by, with exiv2's own spelling. DC-008 wins: it is the specification this crate implements, and
/// exiv2 is the oracle, not the source. See `STATUS.md` for each reading.
const NAME_DIVERGENCES: &[(&str, &str)] = &[
    // 0x02BC: exiv2 follows the XMP specification's name; gamut follows TIFF/EP's.
    ("ApplicationNotes", "XMLPacket"),
    // 0x83BB: the same name, hyphenated as TIFF/EP writes it.
    ("IPTC-NAA", "IPTCNAA"),
    // 0x8827 was renamed from ISOSpeedRatings to PhotographicSensitivity in Exif 2.3; exiv2 keeps
    // the Exif 2.2 name.
    ("PhotographicSensitivity", "ISOSpeedRatings"),
];

#[test]
fn exiv2_knows_every_catalogued_tag_by_the_name_gamut_gives_it() {
    // The catalogue's names are asserted against the reference implementation rather than against
    // a hand-written list, which would only say the table equals itself.
    let mut unknown_to_exiv2 = Vec::new();
    for ifd in [
        IfdKind::Image,
        IfdKind::Exif,
        IfdKind::Gps,
        IfdKind::Interop,
    ] {
        let (tags, bytes) = stream_of_every_tag(ifd);
        assert!(
            exif_count(&bytes).expect("exiv2 decodes the stream") >= tags.len(),
            "exiv2 read fewer tags than gamut wrote into {ifd:?}"
        );
        for tag in tags {
            let key = format!("{}.{}", exiv2_group(ifd), tag.name());
            if exif_get(&bytes, &key).is_err() {
                unknown_to_exiv2.push(tag.name());
            }
        }
    }

    let expected: Vec<&str> = NAME_DIVERGENCES.iter().map(|&(ours, _)| ours).collect();
    assert_eq!(
        unknown_to_exiv2, expected,
        "the set of names exiv2 does not share with gamut changed"
    );
}

#[test]
fn each_divergent_tag_is_the_same_tag_under_exiv2s_own_name() {
    // A divergence must be a naming difference, not a missing tag: exiv2 must read the very value
    // gamut wrote, under its own spelling.
    let (_, image) = stream_of_every_tag(IfdKind::Image);
    let (_, photo) = stream_of_every_tag(IfdKind::Exif);
    for &(ours, theirs) in NAME_DIVERGENCES {
        let (bytes, group) = if ours == "PhotographicSensitivity" {
            (&photo, "Exif.Photo")
        } else {
            (&image, "Exif.Image")
        };
        assert!(
            exif_get(bytes, &format!("{group}.{ours}")).is_err(),
            "exiv2 unexpectedly knows {ours}; it is no longer a divergence"
        );
        assert!(
            exif_get(bytes, &format!("{group}.{theirs}")).is_ok(),
            "exiv2 does not know {theirs} either, so the recorded spelling is wrong"
        );
    }
}

/// The 0th-IFD stream holding every row of a registry block that CIPA DC-008 does not already
/// catalogue — the rows #532 added — written conformantly to the row's own specification.
fn stream_of_registry_rows(block: TagBlock) -> (Vec<&'static TagSpec>, Vec<u8>) {
    let rows: Vec<&'static TagSpec> = block
        .specs()
        .iter()
        .filter(|spec| ExifTag::from_id(IfdKind::Image, spec.id).is_none())
        .collect();
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    for spec in &rows {
        exif.image_mut()
            .set(spec.id, conforming(spec.field_types, spec.count));
    }
    let bytes = ExifWriter::new()
        .marker(false)
        .write(&exif)
        .expect("gamut writes every registry row");
    (rows, bytes)
}

/// Registry rows exiv2 knows under a different name than the specification gives them, with
/// exiv2's own spelling. The vendored specification wins, as for [`NAME_DIVERGENCES`].
const REGISTRY_NAME_DIVERGENCES: &[(&str, &str)] = &[
    // 0x0107: TIFF 6.0 spells it with a doubled "h" in Section 8 and Appendix A alike; exiv2
    // corrects the spelling.
    ("Threshholding", "Thresholding"),
];

/// Registry rows exiv2 0.28 has no `Exif.Image` entry for at all, under any name, so it keys them
/// by number. All four are TIFF 6.0 Section 8 baseline fields. Recorded so an exiv2 that learns
/// one shows up as a change here.
const REGISTRY_ROWS_EXIV2_LACKS: &[&str] = &[
    "MinSampleValue",
    "MaxSampleValue",
    "FreeOffsets",
    "FreeByteCounts",
];

#[test]
fn exiv2_knows_every_registry_row_by_the_name_its_specification_gives_it() {
    // The registry's names are asserted against the reference implementation, block by block,
    // for every row DC-008 does not already cover (those are held by the catalogue test above).
    let mut unknown_to_exiv2 = Vec::new();
    for block in [tags::TIFF6, tags::DNG] {
        let (rows, bytes) = stream_of_registry_rows(block);
        assert!(
            exif_count(&bytes).expect("exiv2 decodes the stream") >= rows.len(),
            "exiv2 read fewer tags than gamut wrote from {}",
            block.specification()
        );
        for spec in rows {
            if exif_get(&bytes, &format!("Exif.Image.{}", spec.name)).is_err() {
                unknown_to_exiv2.push(spec.name);
            }
        }
    }

    let expected: Vec<&str> = REGISTRY_NAME_DIVERGENCES
        .iter()
        .map(|&(ours, _)| ours)
        .chain(REGISTRY_ROWS_EXIV2_LACKS.iter().copied())
        .collect();
    let mut sorted = unknown_to_exiv2.clone();
    sorted.sort_unstable();
    let mut expected_sorted = expected;
    expected_sorted.sort_unstable();
    assert_eq!(
        sorted, expected_sorted,
        "the set of registry names exiv2 does not share changed"
    );
}

#[test]
fn each_divergent_registry_row_is_the_same_tag_under_exiv2s_own_name() {
    let (_, tiff) = stream_of_registry_rows(tags::TIFF6);
    let (_, dng) = stream_of_registry_rows(tags::DNG);
    for &(ours, theirs) in REGISTRY_NAME_DIVERGENCES {
        let bytes = if tags::TIFF6.specs().iter().any(|s| s.name == ours) {
            &tiff
        } else {
            &dng
        };
        assert!(
            exif_get(bytes, &format!("Exif.Image.{theirs}")).is_ok(),
            "exiv2 does not know {theirs} either, so the recorded spelling is wrong"
        );
    }
    // A row exiv2 lacks must still be one it read: it keys an unnamed tag by its number.
    for &name in REGISTRY_ROWS_EXIV2_LACKS {
        let spec = tags::TIFF6
            .specs()
            .iter()
            .chain(tags::DNG.specs())
            .find(|s| s.name == name)
            .expect("a recorded row is a registry row");
        let bytes = if tags::TIFF6.find(spec.id).is_some() {
            &tiff
        } else {
            &dng
        };
        assert!(
            exif_get(bytes, &format!("Exif.Image.0x{:04x}", spec.id)).is_ok(),
            "exiv2 did not read {name} back even by number"
        );
    }
}

#[test]
fn exiv2_knows_the_exif_30_authorship_tags() {
    // The seven tags this branch added are the ones most likely to be mis-transcribed, having no
    // pre-3.0 history: exiv2 resolving each by name and reading the value back is independent
    // confirmation of the transcription.
    let (_, bytes) = stream_of_every_tag(IfdKind::Exif);
    for name in [
        "ImageTitle",
        "Photographer",
        "ImageEditor",
        "CameraFirmware",
        "RAWDevelopingSoftware",
        "ImageEditingSoftware",
        "MetadataEditingSoftware",
    ] {
        assert_eq!(
            exif_get(&bytes, &format!("Exif.Photo.{name}")).as_deref(),
            Ok("A"),
            "exiv2 did not read {name} back"
        );
    }
}
