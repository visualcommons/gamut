//! Parser robustness: malformed, hostile, or out-of-scope inputs must yield a typed error, never
//! a panic or a runaway allocation.

mod common;

use common::{
    av01_item, bx, cat, empty_iprp, ftyp, full, hdlr, iinf_v0, image, infe_v2, meta, pitm_v0,
};
use gamut_core::ErrorKind;
use gamut_isobmff::read;

/// A valid single-item AVIF-style file (written by this crate) to corrupt in place.
fn valid() -> Vec<u8> {
    gamut_isobmff::write(&image(vec![av01_item(1, vec![0xAB; 8])])).unwrap()
}

/// Absolute position of the first occurrence of `fourcc`.
fn find(buf: &[u8], fourcc: &[u8; 4]) -> usize {
    buf.windows(4).position(|w| w == fourcc).unwrap()
}

#[track_caller]
fn assert_read_fails(data: &[u8], expected: &str) {
    let e = read(data).unwrap_err().to_string();
    assert!(e.contains(expected), "expected {expected:?} in {e:?}");
}

#[test]
fn valid_file_reads_back() {
    assert!(read(&valid()).is_ok());
}

#[test]
fn empty_input_errors() {
    assert_read_fails(&[], "missing ftyp");
}

#[test]
fn truncated_box_header_errors() {
    assert!(read(&[0, 0, 0, 8]).is_err());
}

#[test]
fn box_size_below_header_errors() {
    assert_read_fails(
        &[0, 0, 0, 4, b'f', b't', b'y', b'p'],
        "size smaller than header",
    );
}

#[test]
fn missing_meta_errors() {
    assert_read_fails(&ftyp(), "missing meta");
}

#[test]
fn missing_mdat_box_is_tolerated() {
    // Payload extents are absolute file offsets; the mdat framing itself carries no information,
    // so a file whose mdat was renamed to a free box still resolves.
    let mut f = valid();
    let p = find(&f, b"mdat");
    f[p..p + 4].copy_from_slice(b"free");
    assert_eq!(read(&f).unwrap().items[0].payload, vec![0xAB; 8]);
}

#[test]
fn truncated_largesize_errors() {
    assert_read_fails(&[0, 0, 0, 1, b'm', b'd', b'a', b't'], "unexpected end");
}

#[test]
fn largesize_mdat_resolves_the_shifted_payload() {
    let mut f = valid();
    let iloc = find(&f, b"iloc");
    let old_offset = u32::from_be_bytes(f[iloc + 18..iloc + 22].try_into().unwrap());
    f[iloc + 18..iloc + 22].copy_from_slice(&(old_offset + 8).to_be_bytes());

    let mdat = find(&f, b"mdat");
    let size_field = mdat - 4;
    let old_size = u32::from_be_bytes(f[size_field..mdat].try_into().unwrap());
    f[size_field..mdat].copy_from_slice(&1_u32.to_be_bytes());
    f.splice(mdat + 4..mdat + 4, (u64::from(old_size) + 8).to_be_bytes());

    assert_eq!(read(&f).unwrap().items[0].payload, vec![0xAB; 8]);
}

#[test]
fn zero_size_mdat_resolves_payload_through_end_of_input() {
    let mut f = valid();
    let mdat = find(&f, b"mdat");
    f[mdat - 4..mdat].copy_from_slice(&0_u32.to_be_bytes());

    assert_eq!(read(&f).unwrap().items[0].payload, vec![0xAB; 8]);
}

#[test]
fn iloc_extent_out_of_bounds_errors() {
    let mut f = valid();
    let p = find(&f, b"iloc");
    // extent_offset is at body offset 14 → absolute p + 4 + 14.
    f[p + 18..p + 22].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    assert_read_fails(&f, "extent out of bounds");
}

#[test]
fn external_data_reference_is_unsupported() {
    let mut f = valid();
    let p = find(&f, b"iloc");
    // data_reference_index is at body offset 10 → absolute p + 4 + 10.
    f[p + 14..p + 16].copy_from_slice(&[0, 1]);
    assert_read_fails(&f, "external data reference");
}

#[test]
fn ipma_property_index_out_of_range_errors() {
    for bad in [0x7f, 0x00] {
        // Index 127 is far beyond the 4 properties; index 0 is invalid (1-based).
        let mut f = valid();
        let p = find(&f, b"ipma");
        // The first association byte is at body offset 11 → absolute p + 4 + 11.
        f[p + 15] = bad;
        assert_read_fails(&f, "index out of range");
    }
}

#[test]
fn non_picture_handler_is_unsupported() {
    let mut f = valid();
    let p = find(&f, b"pict"); // the hdlr handler_type
    f[p..p + 4].copy_from_slice(b"vide");
    assert_read_fails(&f, "non-picture handler");
}

#[test]
fn missing_hdlr_errors() {
    let mut f = valid();
    let p = find(&f, b"hdlr");
    f[p..p + 4].copy_from_slice(b"xxxx"); // now an ignored unknown meta child
    assert_read_fails(&f, "missing hdlr");
}

#[test]
fn protected_items_are_unsupported() {
    let mut f = valid();
    let p = find(&f, b"infe");
    // item_protection_index is at body offset 6 → absolute p + 4 + 6.
    f[p + 10..p + 12].copy_from_slice(&[0, 1]);
    assert_read_fails(&f, "protected item");
}

#[test]
fn uri_items_are_unsupported() {
    let mut f = valid();
    let p = find(&f, b"infe");
    // item_type is at body offset 8 → absolute p + 4 + 8.
    f[p + 12..p + 16].copy_from_slice(b"uri ");
    assert_read_fails(&f, "uri items");
}

#[test]
fn overlapping_extents_cannot_amplify_the_input() {
    // Hostile multi-extent iloc: 3 extents that each cover (almost) the whole file would resolve
    // to ~3× the input. The reader caps the total resolved payload at the file size.
    let file_covering_extent = cat(&[&0u32.to_be_bytes()[..], &300u32.to_be_bytes()[..]]);
    let iloc = full(
        b"iloc",
        0,
        0,
        &cat(&[
            &[0x44u8, 0x00][..], // offset_size 4, length_size 4, base 0
            &1u16.to_be_bytes(), // item_count
            &1u16.to_be_bytes(), // item_ID
            &0u16.to_be_bytes(), // data_reference_index
            &3u16.to_be_bytes(), // extent_count
            &file_covering_extent,
            &file_covering_extent,
            &file_covering_extent,
        ]),
    );
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iloc,
        iinf_v0(&[infe_v2(1, b"av01")]),
        empty_iprp(),
    ]);
    // Pad with an mdat so a 300-byte extent at offset 0 is in bounds but 3 of them exceed the file.
    let f = cat(&[ftyp(), m, bx(b"mdat", &[0x55; 400])]);
    assert_read_fails(&f, "extents exceed the file size");
}

#[test]
fn construction_method_2_is_unsupported() {
    // iloc v1 with construction_method 2 (item offsets) — structurally valid, out of scope.
    let iloc = full(
        b"iloc",
        1,
        0,
        &cat(&[
            &[0x44u8, 0x00][..],
            &1u16.to_be_bytes(), // item_count
            &1u16.to_be_bytes(), // item_ID
            &2u16.to_be_bytes(), // reserved(12) | construction_method(4) = 2
            &0u16.to_be_bytes(), // data_reference_index
            &1u16.to_be_bytes(), // extent_count
            &0u32.to_be_bytes(),
            &0u32.to_be_bytes(),
        ]),
    );
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iloc,
        iinf_v0(&[infe_v2(1, b"av01")]),
        empty_iprp(),
    ]);
    assert_read_fails(&cat(&[ftyp(), m]), "construction_method 2");
}

#[test]
fn idat_reference_without_idat_errors() {
    // iloc v1, construction_method 1, but the meta carries no idat box.
    let iloc = full(
        b"iloc",
        1,
        0,
        &cat(&[
            &[0x44u8, 0x00][..],
            &1u16.to_be_bytes(),
            &1u16.to_be_bytes(),
            &1u16.to_be_bytes(), // construction_method 1 (idat)
            &0u16.to_be_bytes(),
            &1u16.to_be_bytes(),
            &0u32.to_be_bytes(),
            &4u32.to_be_bytes(),
        ]),
    );
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iloc,
        iinf_v0(&[infe_v2(1, b"av01")]),
        empty_iprp(),
    ]);
    assert_read_fails(&cat(&[ftyp(), m]), "idat but meta has none");
}

#[test]
fn iref_from_unknown_item_errors() {
    let iref = full(
        b"iref",
        0,
        0,
        &bx(
            b"cdsc",
            &cat(&[
                &99u16.to_be_bytes()[..], // from_item_ID: no such item
                &1u16.to_be_bytes(),      // reference_count
                &1u16.to_be_bytes(),
            ]),
        ),
    );
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iinf_v0(&[infe_v2(1, b"av01")]),
        iref,
        empty_iprp(),
    ]);
    assert_read_fails(&cat(&[ftyp(), m]), "iref from unknown item");
}

#[test]
fn mime_infe_missing_content_type_errors() {
    let bad_infe = full(
        b"infe",
        2,
        0,
        &cat(&[&1u16.to_be_bytes()[..], &[0, 0], b"mime", &[0]]), // name only, no content_type
    );
    let m = meta(&[hdlr(), pitm_v0(1), iinf_v0(&[bad_infe]), empty_iprp()]);
    assert_read_fails(&cat(&[ftyp(), m]), "missing content_type");
}

#[test]
fn iloc_field_size_must_be_0_4_or_8() {
    let iloc = full(
        b"iloc",
        0,
        0,
        &cat(&[&[0x34u8, 0x00][..], &0u16.to_be_bytes()]),
    );
    let m = meta(&[hdlr(), pitm_v0(1), iinf_v0(&[]), iloc, empty_iprp()]);
    assert_read_fails(&cat(&[ftyp(), m]), "field size not 0/4/8");
}

// ---- motion-photo tolerance (issue #238) ---------------------------------------------------

/// A second top-level `ftyp` with a distinct major brand.
fn second_ftyp() -> Vec<u8> {
    bx(b"ftyp", b"heic\x00\x00\x00\x00")
}

#[test]
fn appended_motion_photo_stream_is_ignored() {
    // A Samsung-style motion photo: the valid still image, then a *second* ftyp, a foreign moov,
    // and trailing garbage. The primary-stream walk stops at the second ftyp, so the model is
    // byte-for-byte identical to the bare file — the moov and the garbage are never walked.
    let bare = valid();
    let mut motion = bare.clone();
    motion.extend_from_slice(&second_ftyp());
    motion.extend_from_slice(&bx(b"moov", &[0u8; 16]));
    motion.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // non-box garbage
    assert_eq!(read(&motion).unwrap(), read(&bare).unwrap());
}

#[test]
fn trailing_non_box_garbage_is_tolerated() {
    // A trailing vendor blob that is not a box (e.g. a Samsung SEF trailer): once ftyp+meta are
    // seen, a malformed top-level box stops the walk cleanly instead of erroring.
    let bare = valid();
    let mut f = bare.clone();
    f.extend_from_slice(b"SEFT\x00\x01\x02\x03trailing-junk");
    assert_eq!(read(&f).unwrap(), read(&bare).unwrap());
}

#[test]
fn malformed_box_before_meta_still_errors() {
    // A truncated box between ftyp and meta: ftyp is seen but meta is not, so the *walk* error
    // propagates verbatim. Asserting the exact box-walk message (not just `is_err`) pins the
    // tolerance guard `ftyp.is_some() && meta_body.is_some()`: relaxing `&&` to `||` would break the
    // walk cleanly and surface "missing meta" instead — a different error.
    let truncated = [0x00, 0x00, 0x00, 0xFF, b'j', b'u', b'n', b'k']; // claims 255 bytes, has 0
    assert_read_fails(&cat(&[ftyp(), truncated.to_vec()]), "unexpected end of box");
}

#[test]
fn malformed_box_after_meta_without_ftyp_still_errors() {
    // The mirror of the above: `meta` is seen but `ftyp` is not. The truncated trailing box must
    // still propagate the walk error — the `&&` tolerance guard requires *both* required boxes, so
    // the `||` mutant (which would surface "missing ftyp") is killed by asserting the walk message.
    let truncated = [0x00, 0x00, 0x00, 0xFF, b'j', b'u', b'n', b'k']; // claims 255 bytes, has 0
    assert_read_fails(
        &cat(&[meta(&[]), truncated.to_vec()]),
        "unexpected end of box",
    );
}

// ---- sequence tracks beside the still (issue #646) ------------------------------------------

/// The `meta` of a one-item still with no payload (no `iloc`), so boxes can be placed anywhere
/// around it without shifting an absolute extent offset.
fn still_meta() -> Vec<u8> {
    meta(&[
        hdlr(),
        pitm_v0(1),
        iinf_v0(&[infe_v2(1, b"av01")]),
        empty_iprp(),
    ])
}

#[test]
fn track_beside_meta_is_skipped_not_retained() {
    // An `avis`-shaped file: ftyp, meta, then the sequence track, then mdat. The still reads, and
    // the track is neither an error nor a retained top-level box. Each alternative of the
    // `moov | trak` arm is asserted on its own: a dropped alternative would retain that box.
    for ty in [b"moov", b"trak"] {
        let f = cat(&[ftyp(), still_meta(), bx(ty, &[0u8; 16]), bx(b"mdat", &[])]);
        let model = read(&f).unwrap();
        assert_eq!(model.primary_item_id, 1, "{ty:?}");
        assert!(model.top_level_boxes.is_empty(), "{ty:?} retained");
    }
}

#[test]
fn track_without_meta_is_a_sequence_only_error() {
    // A track and no `meta`: a sequence with no still item, refused as Unsupported with a specific
    // message rather than the generic InvalidInput "missing meta" a file with neither carries.
    for ty in [b"moov", b"trak"] {
        let e = read(&cat(&[ftyp(), bx(ty, &[0u8; 16])])).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Unsupported, "{ty:?}");
        assert!(e.to_string().contains("sequence-only"), "{ty:?}: {e}");
    }
    assert_eq!(read(&ftyp()).unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn first_ftyp_wins_over_appended_ftyp() {
    // The appended stream's ftyp has a different major brand; the primary brand must be unchanged.
    let mut f = valid();
    f.extend_from_slice(&second_ftyp()); // major brand heic
    let model = read(&f).unwrap();
    assert_eq!(&model.major_brand, b"avif", "first (primary) ftyp wins");
}

#[test]
fn iloc_extent_into_appended_region_still_resolves() {
    // Hand-author a file whose single item's payload lives *after* a second ftyp (in the appended
    // region). Payload resolution addresses the full buffer, so the absolute extent still resolves.
    let payload = [0x11u8, 0x22, 0x33, 0x44];
    let iloc = full(
        b"iloc",
        0,
        0,
        &cat(&[
            &[0x44u8, 0x00][..],                   // offset_size 4, length_size 4, base 0
            &1u16.to_be_bytes(),                   // item_count
            &1u16.to_be_bytes(),                   // item_ID
            &0u16.to_be_bytes(),                   // data_reference_index
            &1u16.to_be_bytes(),                   // extent_count
            &0u32.to_be_bytes(),                   // extent_offset (patched below)
            &(payload.len() as u32).to_be_bytes(), // extent_length
        ]),
    );
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iloc,
        iinf_v0(&[infe_v2(1, b"av01")]),
        empty_iprp(),
    ]);
    let mut f = cat(&[ftyp(), m, second_ftyp()]);
    let payload_abs = f.len();
    f.extend_from_slice(&payload); // appended after the second ftyp
    // Patch the iloc extent_offset (body offset 14 → absolute find("iloc") + 4 + 14) to point at it.
    let ep = find(&f, b"iloc") + 18;
    f[ep..ep + 4].copy_from_slice(&(payload_abs as u32).to_be_bytes());

    let model = read(&f).unwrap();
    assert_eq!(model.items[0].payload, payload.to_vec());
}
