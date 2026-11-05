//! Byte-completeness fidelity matrix (issue #263): every representable value survives a
//! read → write → read round-trip, unknown/vendor material is preserved byte-exactly, large
//! out-of-line values hold together, and the maker-note byte range can be kept untouched.

use gamut_ifd::{
    ByteOrder, Ifd, TiffFile, UnknownValue, Value, Variant, WriteOptions, read, read_audited, tags,
    write, write_with,
};

fn orders_and_variants() -> Vec<(ByteOrder, Variant)> {
    let classic = vec![
        (ByteOrder::LittleEndian, Variant::Classic),
        (ByteOrder::BigEndian, Variant::Classic),
    ];
    #[cfg(not(feature = "bigtiff"))]
    return classic;
    #[cfg(feature = "bigtiff")]
    {
        let mut all = classic;
        all.push((ByteOrder::LittleEndian, Variant::Big));
        all.push((ByteOrder::BigEndian, Variant::Big));
        all
    }
}

/// A directory carrying one field of **every** representable value shape — every TIFF 6.0 type,
/// the TechNote-1 `IFD` type, Exif 3.0 `UTF8`, an unknown-type record, and (with `bigtiff`) the
/// 64-bit types — with inline and out-of-line lengths mixed.
fn every_type_ifd(order: ByteOrder, variant: Variant) -> Ifd {
    let mut ifd = Ifd::new();
    let mut tag = 0x8000u16;
    let mut next = |value: Value, ifd: &mut Ifd| {
        ifd.set(tag, value);
        tag += 1;
    };
    next(Value::Byte(vec![1, 2, 3]), &mut ifd);
    next(Value::Ascii("first\0second".to_owned()), &mut ifd);
    next(Value::Short(vec![0xABCD, 1]), &mut ifd);
    next(Value::Long(vec![0xDEAD_BEEF]), &mut ifd);
    next(Value::Rational(vec![(300, 1), (72, 1)]), &mut ifd);
    next(Value::SByte(vec![-1, 127, -128]), &mut ifd);
    next(
        Value::Undefined(vec![0xFF, 0x00, 0x7F, 0x80, 0x01]),
        &mut ifd,
    );
    next(Value::SShort(vec![-32768, 32767]), &mut ifd);
    next(Value::SLong(vec![i32::MIN, i32::MAX]), &mut ifd);
    next(Value::SRational(vec![(-1, 2), (3, -4)]), &mut ifd);
    next(Value::Float(vec![1.5, -0.25, f32::MIN_POSITIVE]), &mut ifd);
    next(Value::Double(vec![1.5, -0.062_5]), &mut ifd);
    next(Value::Ifd(vec![0]), &mut ifd); // a (dangling) typed IFD pointer value
    next(Value::Utf8("café — 日本語".to_owned()), &mut ifd);
    let word: &[u8] = match variant {
        Variant::Classic => &[0xDE, 0xAD, 0xBE, 0xEF],
        // BigTIFF's 8-byte word (`Variant` is non-exhaustive, so the arm is a wildcard).
        _ => &[1, 2, 3, 4, 5, 6, 7, 8],
    };
    next(
        Value::Unknown(UnknownValue::new(0xF0, 3, word, order, variant).expect("capture")),
        &mut ifd,
    );
    #[cfg(feature = "bigtiff")]
    if variant == Variant::Big {
        next(Value::Long8(vec![0x1_0000_0000, u64::MAX]), &mut ifd);
        next(Value::SLong8(vec![i64::MIN, -1]), &mut ifd);
        next(Value::Ifd8(vec![16]), &mut ifd);
    }
    ifd
}

/// Every value shape round-trips through a written file, in every order × variant, and the
/// rewrite of the parse is byte-identical to the first write (the fixpoint: nothing decays
/// across repeated rewrites).
#[test]
fn every_field_type_roundtrips_through_a_file() {
    for (order, variant) in orders_and_variants() {
        let file = TiffFile {
            order,
            variant,
            ifds: vec![every_type_ifd(order, variant)],
        };
        let bytes = write(&file).expect("write");
        let parsed = read(&bytes).expect("read");
        assert_eq!(parsed, file, "{order:?} {variant:?}");
        let rewrite = write(&parsed).expect("rewrite");
        assert_eq!(rewrite, bytes, "rewrite fixpoint {order:?} {variant:?}");
    }
}

/// A ≥1 MiB out-of-line value: written, fully classified by the audit, and byte-exact through
/// the round-trip.
#[test]
fn large_out_of_line_value_roundtrips_and_classifies() {
    let payload: Vec<u8> = (0..1_048_577u32).map(|i| (i % 251) as u8).collect();
    let mut ifd = Ifd::new();
    ifd.set(700, Value::Undefined(payload.clone()));
    ifd.set(256, Value::Short(vec![1]));
    let file = TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd],
    };
    let bytes = write(&file).expect("write");
    let (parsed, report) = read_audited(&bytes).expect("read");
    assert_eq!(parsed, file);
    assert!(report.is_fully_classified(), "large value fully claimed");
    assert_eq!(
        parsed.ifds[0].get(700).and_then(Value::as_bytes),
        Some(&payload[..])
    );
}

/// The maker-note requirement end to end: a vendor blob whose internal offsets are absolute
/// (so relocation would corrupt it) is (a) byte-exact at the value level through an ordinary
/// rewrite, and (b) kept at its **original absolute offset** through a pinned rewrite, so the
/// internal offsets stay valid.
#[test]
fn maker_note_bytes_survive_and_pin_at_their_source_offset() {
    // The "maker note": a mini-TIFF whose value offsets are absolute within the note.
    let mut note_ifd = Ifd::new();
    note_ifd.set(1, Value::Short(vec![7]));
    note_ifd.set(2, Value::Ascii("vendor mode".to_owned()));
    let note = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![note_ifd],
    })
    .expect("write note");

    let mut exif = Ifd::new();
    exif.set(tags::MAKER_NOTE, Value::Undefined(note.clone()));
    exif.set(33434, Value::Rational(vec![(1, 100)]));
    let mut root = Ifd::new();
    root.set(256, Value::Short(vec![640]));
    root.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
    let file = TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![root],
    };

    // (a) Value-level byte exactness through an ordinary rewrite.
    let bytes = write(&file).expect("write");
    let parsed = read(&bytes).expect("read");
    let exif_off = u64::from(parsed.ifds[0].get_u32(tags::EXIF_IFD).expect("pointer"));
    let exif_ifd =
        gamut_ifd::read_ifd_at(&bytes, exif_off, file.order, file.variant).expect("exif");
    assert_eq!(
        exif_ifd.get(tags::MAKER_NOTE).and_then(Value::as_bytes),
        Some(&note[..])
    );

    // (b) Offset preservation through a pinned rewrite: find the note's current absolute
    // position, then rewrite pinning it there — the bytes land at exactly that offset.
    let note_at = find(&bytes, &note).expect("note present") as u64;
    let (pinned_bytes, map) = write_with(
        &file,
        &WriteOptions::default().pin(tags::MAKER_NOTE, note_at),
    )
    .expect("pinned write");
    assert_eq!(
        &pinned_bytes[note_at as usize..note_at as usize + note.len()],
        &note[..],
        "the maker-note byte range is untouched at its source offset"
    );
    assert!(map.finish(None).is_fully_classified());
    // And the pinned stream still parses to the same tree.
    assert_eq!(
        gamut_ifd::IfdReader::open(&pinned_bytes[..])
            .and_then(|mut r| r.read_tree(tags::STANDARD_POINTER_TAGS))
            .expect("read pinned"),
        file
    );
}

/// Naive subsequence search (the fixtures are tiny).
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Unknown/vendor tags with *known* types keep their value bytes exactly, even out of line.
#[test]
fn vendor_tag_values_are_byte_exact_after_rewrite() {
    let vendor_blob: Vec<u8> = (0..999u32).map(|i| (i % 256) as u8).collect();
    let mut ifd = Ifd::new();
    ifd.set(0xEA1C, Value::Undefined(vendor_blob.clone())); // a real-world vendor padding tag
    ifd.set(0x9C9B, Value::Byte(vec![0x58, 0x00, 0x50, 0x00])); // XP* style vendor tag
    let file = TiffFile {
        order: ByteOrder::BigEndian,
        variant: Variant::Classic,
        ifds: vec![ifd],
    };
    let bytes = write(&file).expect("write");
    let reparsed = read(&write(&read(&bytes).expect("read")).expect("rewrite")).expect("reread");
    assert_eq!(
        reparsed.ifds[0].get(0xEA1C).and_then(Value::as_bytes),
        Some(&vendor_blob[..])
    );
    assert_eq!(reparsed, file);
}
