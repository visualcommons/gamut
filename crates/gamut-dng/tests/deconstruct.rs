//! Strict deconstruct (issue #197): encode a DNG, then prove the deconstruct accounts every byte
//! of the IFD tree (IFD 0 preview, the raw sub-IFD, Exif) and flags nothing unexpected.

mod common;

use gamut_dng::{Anomaly, ByteOrder, DeconstructReport, DngDecoder, DngEncoder, deconstruct};

/// Asserts the structural walk classified the whole file with nothing unrecognised —
/// **zero tolerance**: word-alignment padding must come back as typed `Padding` segments,
/// not tolerated gaps (issue #263).
fn assert_clean(r: &DeconstructReport) {
    assert!(r.is_fully_classified(), "not fully classified: {r:?}");
    assert!(
        r.segments.is_fully_classified(),
        "not fully classified: {r:?}"
    );
    assert!(r.unknown_fields.is_empty(), "unknown fields: {r:?}");
    assert!(
        r.unknown_tags.is_empty(),
        "unknown tags: {:?}",
        r.unknown_tags
    );
    assert!(r.anomalies.is_empty(), "anomalies: {:?}", r.anomalies);
    assert!(r.is_fully_accounted(), "not fully accounted: {r:?}");
}

#[test]
fn encoded_cfa_dng_is_accounted() {
    for &order in &[ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        for bits in [12u16, 16] {
            let raw = common::sample_raw(32, 24, bits);
            let mut dng = Vec::new();
            DngEncoder::new()
                .with_byte_order(order)
                .encode(&raw, &common::sample_profile(), &mut dng)
                .expect("encode");
            let report = deconstruct(&dng).expect("deconstruct");
            assert_clean(&report);
            // The raw sub-IFD's strips are the bulk of the file — the Data segments must
            // cover most of it.
            let data_bytes: u64 = report
                .segments
                .segments
                .iter()
                .filter(|s| matches!(s.kind, gamut_ifd::SpanKind::Data(_)))
                .map(|s| s.range.len)
                .sum();
            assert!(data_bytes * 2 > report.segments.file_len, "{report:?}");
        }
    }
}

#[test]
fn full_profile_dng_is_accounted() {
    // The full profile exercises the optional matrix / calibration tags the matrix check inspects.
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile_full(), &mut dng)
        .expect("encode");
    let report = deconstruct(&dng).expect("deconstruct");
    assert_clean(&report);
}

#[test]
fn linear_raw_dng_is_accounted() {
    let raw = common::sample_linear_raw(20, 12, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let report = deconstruct(&dng).expect("deconstruct");
    assert_clean(&report);
}

/// A C2PA manifest store (C2PA 2.4 §A.3.6) is placed after the image data, last in the file,
/// and the byte accounting claims it as the value of IFD 0's tag-52545 entry — never as an
/// unclassified run or a trailer. The whole report is clean, on the same terms as every other
/// file this encoder writes: the store's tag is a tag this crate knows, so `is_fully_accounted`
/// stays true rather than reporting the file's own manifest store as a private tag.
#[test]
fn a_c2pa_store_at_the_end_of_the_file_is_the_entrys_value_span() {
    use gamut_dng::{DngMetadata, Segment, SpanKind};
    use gamut_ifd::c2pa::C2PA_MANIFEST_STORE;

    for &order in &[ByteOrder::LittleEndian, ByteOrder::BigEndian] {
        // An 8-bit 33×23 mosaic: an odd-length raw strip, so the store needs alignment filler.
        let raw = common::sample_raw(33, 23, 8);
        let mut dng = Vec::new();
        let report = DngEncoder::new()
            .with_byte_order(order)
            .with_metadata(DngMetadata {
                c2pa: Some((0u8..40).collect()),
                ..Default::default()
            })
            .encode_with_report(&raw, &common::sample_profile(), &mut dng)
            .expect("encode");
        let excl = report.c2pa.expect("ranges");
        let (_, _, ifd0) = gamut_ifd::read_header(&dng).expect("header");

        let report = deconstruct(&dng).expect("deconstruct");
        assert_clean(&report);
        assert!(
            report.segments.segments.contains(&Segment {
                range: excl.store,
                kind: SpanKind::Value {
                    ifd: ifd0,
                    tag: C2PA_MANIFEST_STORE,
                },
            }),
            "{order:?}: the store is IFD 0's value span: {report:?}"
        );
        assert!(
            !report
                .segments
                .segments
                .iter()
                .any(|s| s.kind == SpanKind::Trailer),
            "{order:?}: a store at the end of the file is not a trailer"
        );
    }
}

#[test]
fn decoder_deconstruct_returns_image_and_report() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let (decoded, report) = DngDecoder::new().deconstruct(&dng).expect("deconstruct");
    // The decoded raw matches a plain decode, and the report comes alongside it.
    assert_eq!(decoded.raw, raw);
    assert_clean(&report);
}

/// A strip declared past the end of the file leaves the verdict false.
///
/// Every assertion on `is_fully_classified` expected `true`, so replacing it with the constant
/// `true` satisfied all of them (#110) -- the suite pinned that clean files are clean and never
/// that a broken one is not. The same gap, and the same fixture shape, as gamut-tiff's.
#[test]
fn a_strip_past_the_end_of_file_is_not_fully_classified() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    // A strip at offset 100000 in a file of a few hundred bytes: the walk can name the segment
    // but cannot place it, which is `out_of_bounds` -- one of the five conditions the verdict is
    // built from.
    ifd0.set(273, Value::Long(vec![100_000])); // StripOffsets
    ifd0.set(279, Value::Long(vec![16])); // StripByteCounts
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");

    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        !report.is_fully_classified(),
        "a strip past EOF must fail the archival verdict: {report:?}"
    );
    assert_eq!(report.segments.out_of_bounds.len(), 1, "{report:?}");
}

#[test]
fn unknown_private_tag_is_flagged() {
    // Inject a private tag into the encoded stream by editing IFD 0 through a re-parse/re-write is
    // awkward; instead, build a minimal raw IFD by hand and confirm a private tag on it is flagged
    // while its bytes stay accounted.
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    // A minimal IFD 0: declare DNGVersion plus a private (unknown) tag.
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(0x9999, Value::Long(vec![42])); // private/unknown tag
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.unknown_tags.iter().any(|u| u.tag == 0x9999),
        "{report:?}"
    );
    assert!(!report.is_fully_accounted());
    // The unknown tag's value is inline, so byte classification is unaffected.
    assert!(report.segments.is_fully_classified());
}

#[test]
fn unknown_compression_code_is_flagged() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(259, Value::Short(vec![999])); // Compression: not a recognised DNG code
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::UnknownCode { tag, code, .. } if *tag == 259 && *code == 999
        )),
        "{report:?}"
    );
}

/// Bytes appended after everything the file accounts for are a **trailer**: named, reported, and
/// never silently absorbed into a neighbouring structure. Real cameras append them (a Leica M10
/// sample carries 651 KB), so the report has to describe them rather than just refuse the file.
#[test]
fn trailing_junk_is_classified_as_a_trailer() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let clean_len = dng.len() as u64;
    dng.extend_from_slice(&[9, 9, 9]);

    let report = deconstruct(&dng).expect("deconstruct");
    assert_eq!(report.segments.unclassified_bytes(), 0);
    assert!(report.is_fully_classified(), "{report:?}");

    let spans = report.segments.unclaimed_spans();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].kind, gamut_dng::SpanKind::Trailer);
    assert_eq!(spans[0].range.start, clean_len);
    assert_eq!(spans[0].range.len, 3);
    assert_eq!(report.segments.unclaimed_span_bytes(), 3);
}

/// A gamut-encoded file accounts for every one of its own bytes, so nothing is named by the
/// position pass — the trailer test above is measuring the appended bytes, not a baseline.
#[test]
fn a_clean_file_has_no_unaccounted_spans() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let report = deconstruct(&dng).expect("deconstruct");
    assert!(report.is_fully_classified(), "{report:?}");
    assert!(report.segments.unclaimed_spans().is_empty());
    assert!(report.is_fully_accounted());
}

#[test]
fn private_tag_inside_the_raw_sub_ifd_is_flagged() {
    // The tag check must recurse through the `SubIFDs` image children — and only those (the
    // Exif sub-IFD's own namespace is exempt).
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut raw_ifd = gamut_ifd::Ifd::new();
    raw_ifd.set(256, Value::Short(vec![4])); // ImageWidth (known)
    raw_ifd.set(273, Value::Long(vec![0])); // StripOffsets placeholder
    raw_ifd.set(279, Value::Long(vec![0]));
    raw_ifd.set(0x9AAA, Value::Short(vec![1])); // private/unknown
    let mut exif = gamut_ifd::Ifd::new();
    exif.set(33434, Value::Rational(vec![(1, 100)])); // EXIF namespace: must NOT be flagged
    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set_sub_ifd(330, vec![raw_ifd]);
    ifd0.set_sub_ifd(34665, vec![exif]);
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.unknown_tags.iter().any(|u| u.tag == 0x9AAA),
        "{report:?}"
    );
    assert!(
        report.unknown_tags.iter().all(|u| u.tag != 33434),
        "EXIF-namespace tags are not DNG-unknown: {report:?}"
    );
}

#[test]
fn tiled_image_missing_tile_pair_is_flagged() {
    // TileWidth alone declares a tiled image; the missing TileOffsets/TileByteCounts pair must
    // surface as the *tile* anomaly, not fall through to the strip/no-data branches.
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(256, Value::Short(vec![4])); // ImageWidth
    ifd0.set(322, Value::Short(vec![16])); // TileWidth, but no offsets/counts
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure { detail, severity: gamut_dng::Severity::Error, .. }
                if detail.contains("Tile")
        )),
        "{report:?}"
    );
}

#[test]
fn chained_sub_ifd_is_followed_and_warned() {
    // A SubIFDs child carrying an out-of-spec next-IFD link: followed and accounted, with a
    // warning anomaly.
    let data: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
        0x01, 0x00, // IFD0: 1 entry
        0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, // 330 -> 26
        0x00, 0x00, 0x00, 0x00, // next = 0
        0x01, 0x00, // child A @ 26
        0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // 256 = 1
        0x2c, 0x00, 0x00, 0x00, // next = 44 (out of spec)
        0x01, 0x00, // child B @ 44
        0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // 256 = 2
        0x00, 0x00, 0x00, 0x00, // next = 0
    ];
    let report = deconstruct(data).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure { detail, severity: gamut_dng::Severity::Warning, .. }
                if detail.contains("next-IFD chain")
        )),
        "{report:?}"
    );
    assert!(report.segments.is_fully_classified(), "{report:?}");
}

#[test]
fn sub_ifd_skip_reasons_map_to_distinct_details() {
    // A cycle names the cycle; a depth bomb names the depth — the mapped details must stay
    // distinct diagnoses, not collapse into the generic "could not be parsed".
    let cyclic: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00,
    ];
    let report = deconstruct(cyclic).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure { detail, .. } if detail.contains("cycle")
        )),
        "{report:?}"
    );

    use gamut_ifd::{TiffFile, Value, Variant, write};
    let mut ifd = gamut_ifd::Ifd::new();
    ifd.set(256, Value::Short(vec![1]));
    for _ in 0..20 {
        let mut parent = gamut_ifd::Ifd::new();
        parent.set_sub_ifd(330, vec![ifd]);
        ifd = parent;
    }
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure { detail, .. } if detail.contains("too deep")
        )),
        "{report:?}"
    );
}

#[test]
fn big_endian_camera_profile_is_walked_cleanly() {
    // A valid big-endian `.dcp`-form profile stream: header BOM `MM`, magic 0x4352, then a
    // stream-relative directory — built by writing a BE mini-TIFF and patching its magic.
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut profile = gamut_ifd::Ifd::new();
    profile.set(50936, Value::Ascii("Standard".into())); // ProfileName
    let mut stream = write(&TiffFile {
        order: ByteOrder::BigEndian,
        variant: Variant::Classic,
        ifds: vec![profile],
    })
    .expect("write profile");
    // Patch the TIFF magic (42) to the camera-profile magic (0x4352), big-endian.
    stream[2] = 0x43;
    stream[3] = 0x52;

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(50933, Value::Long(vec![0])); // ExtraCameraProfiles: patched below
    let probe = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0.clone()],
    })
    .expect("probe");
    let base = gamut_ifd::align_word(probe.len() as u64);
    ifd0.set(50933, Value::Long(vec![base as u32]));
    let mut bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    bytes.resize(base as usize, 0);
    bytes.extend_from_slice(&stream);

    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(report.anomalies.is_empty(), "{report:?}");
    assert!(report.segments.is_fully_classified(), "{report:?}");
}

#[test]
fn missing_strip_byte_counts_is_flagged() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(256, Value::Short(vec![4])); // ImageWidth: an image IFD...
    ifd0.set(273, Value::Long(vec![1000])); // ...with StripOffsets but no StripByteCounts
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure {
                severity: gamut_dng::Severity::Error,
                ..
            }
        )),
        "{report:?}"
    );
}

#[test]
fn image_ifd_without_pixel_data_warns() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(256, Value::Short(vec![4])); // ImageWidth, but neither strips nor tiles
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure {
                severity: gamut_dng::Severity::Warning,
                ..
            }
        )),
        "{report:?}"
    );
}

#[test]
fn cyclic_sub_ifd_is_flagged_not_hung() {
    // Root @8 whose SubIFDs pointer (330) targets a child @26 pointing back at itself.
    let data: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00,
    ];
    let report = deconstruct(data).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure {
                severity: gamut_dng::Severity::Error,
                ..
            }
        )),
        "{report:?}"
    );
}

#[test]
fn unparseable_camera_profile_is_flagged() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    // ExtraCameraProfiles pointing at bytes that are not a camera-profile stream.
    ifd0.set(50933, Value::Long(vec![4]));
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::Structure {
                severity: gamut_dng::Severity::Error,
                ..
            }
        )),
        "{report:?}"
    );
}

#[test]
fn unknown_field_type_entry_is_reported_and_preserved() {
    // A hand-patched unknown field-type code (0xF0) in IFD 0: reported in `unknown_fields`,
    // while the byte-level classification stays clean (the record sits inside the body claim).
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    ifd0.set(0x9999, Value::Long(vec![42]));
    let mut bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    // Entry records start at 10 (header 8 + count 2), sorted by tag — 0x9999 (39321) sorts
    // before DNGVersion (50706), so its type code sits at 10 + 2.
    bytes[12] = 0xF0;
    let report = deconstruct(&bytes).expect("deconstruct");
    assert_eq!(report.unknown_fields.len(), 1, "{report:?}");
    assert_eq!(report.unknown_fields[0].tag, 0x9999);
    assert_eq!(report.unknown_fields[0].type_code, 0xF0);
    assert!(report.segments.is_fully_classified(), "{report:?}");
    assert!(!report.is_fully_accounted());
}

#[test]
fn malformed_color_matrix_is_flagged() {
    use gamut_ifd::{TiffFile, Value, Variant, write};

    let mut ifd0 = gamut_ifd::Ifd::new();
    ifd0.set(50706, Value::Byte(vec![1, 7, 0, 0])); // DNGVersion
    // ColorMatrix1 (50721) must be nine rationals; give it three.
    ifd0.set(50721, Value::SRational(vec![(1, 1), (0, 1), (0, 1)]));
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");
    let report = deconstruct(&bytes).expect("deconstruct");
    assert!(
        report.anomalies.iter().any(|a| matches!(
            a,
            Anomaly::UnparsableTag { tag, .. } if *tag == 50721
        )),
        "{report:?}"
    );
}
