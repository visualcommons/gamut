//! Locating the C2PA manifest store: the reported range must cover the store *exactly* (every byte
//! of framing excluded), every rejection branch must yield `None` rather than an error, and a `uuid`
//! box that is not a top-level C2PA `ContentProvenanceBox` must never be reported as one.
//!
//! A box that *is* a `ContentProvenanceBox` and still yields no store is a separate outcome from a
//! file carrying none, and `c2pa_summary` must keep them apart; the `unread` cases below pin which
//! reason each malformed shape earns.
//!
//! Fixtures are hand-built so the byte offsets are known by construction and can be asserted as
//! literals; C2PA clause references are to the 2.4 specification.

mod common;

use common::{
    C2PA_UUID, bx, c2pa_box, cat, clean_file, ftyp, hdlr, hvc1_item, iinf_v0, infe_v2, jumbf_store,
    meta, pitm_v0, uuid_box,
};
use gamut_heic::{
    C2paBoxPosition, C2paBoxPurpose, C2paUnreadReason, HeifContainer, UnknownBoxLocation,
};

/// The `meta` box every fixture below closes with: the minimum that `HeifContainer::parse` accepts
/// (a `pict` handler, a primary item, and that item's `infe`).
fn minimal_meta() -> Vec<u8> {
    meta(&[hdlr(), pitm_v0(1), iinf_v0(&[infe_v2(1, b"hvc1", false)])])
}

/// `ftyp` + the given top-level boxes + `meta`, i.e. the placement C2PA 2.4 §A.5.3 mandates (after
/// `ftyp`, before any `mdat`). The `ftyp` here is exactly 16 bytes, so the first extra box's header
/// starts at offset 16.
fn file_with(top_level: &[Vec<u8>]) -> Vec<u8> {
    let mut parts = vec![ftyp(b"heic")];
    parts.extend_from_slice(top_level);
    parts.push(minimal_meta());
    cat(&parts)
}

/// Offset of the first box after the 16-byte `ftyp`.
const AFTER_FTYP: usize = 16;

/// A recognisable opaque manifest store: a JUMBF superbox 29 bytes long.
fn store() -> Vec<u8> {
    jumbf_store(b"opaque-manifest-store")
}

#[test]
fn manifest_store_range_excludes_every_byte_of_framing() {
    let store = store();
    assert_eq!(store.len(), 29, "fixture store length is load-bearing");
    // 8 bytes of padding after the store: §A.5.3 allows them, and they must not be reported.
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[0xEE; 8])]);
    let c = HeifContainer::parse(&data).unwrap();

    // 16 (ftyp) + 8 (box header) + 16 (user type) + 4 (FullBox version+flags)
    //   + 9 ("manifest\0") + 8 (merkle offset) = 61.
    let start = AFTER_FTYP + 8 + 16 + 4 + 9 + 8;
    assert_eq!(start, 61);

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.range, 61..90);
    assert_eq!(found.bytes, store.as_slice());
    assert_eq!(found.purpose, C2paBoxPurpose::Manifest);
    // The range is an index into the file, not just a length: the bytes it names *are* the store.
    assert_eq!(&data[found.range.clone()], store.as_slice());
    // The byte immediately before the store is the last byte of the merkle offset, and the byte
    // immediately after is the first padding byte — the range is tight on both ends.
    assert_eq!(data[found.range.start - 1], 0x00);
    assert_eq!(data[found.range.end], 0xEE);
}

#[test]
fn non_zero_merkle_offset_is_still_excluded_from_the_range() {
    let store = store();
    let data = file_with(&[c2pa_box(
        "manifest",
        Some(0x0102_0304_0506_0708),
        &store,
        &[],
    )]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.range, 61..90);
    assert_eq!(found.bytes, store.as_slice());
    // The eight merkle-offset bytes sit immediately before the store, outside the range.
    assert_eq!(&data[53..61], &0x0102_0304_0506_0708_u64.to_be_bytes());
}

#[test]
fn original_purpose_carries_the_merkle_offset_too() {
    let store = store();
    let data = file_with(&[c2pa_box("original", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("original store located");
    assert_eq!(found.purpose, C2paBoxPurpose::Original);
    // "original" is the same length as "manifest", so an identical start proves the 8-byte merkle
    // offset was skipped here as well (§A.5.3: "the 'uuid' box of type manifest or original").
    assert_eq!(found.range, 61..90);
    assert_eq!(found.bytes, store.as_slice());
}

#[test]
fn manifest_purpose_is_not_probed_and_needs_its_stated_merkle_offset() {
    // §A.5.3 states the framing for `manifest`, so there is nothing to resolve and no fallback —
    // the asymmetry against `update` below. Note what this does *not* prove: the single offset is
    // no more self-checking than a probed one, and this box is unreported only because its bytes at
    // offset 8 happen not to read as a valid `LBox`. An out-of-spec `manifest` box whose bytes do
    // would be mis-bounded, exactly as `C2paBoxPurpose` documents for `update`.
    let data = file_with(&[c2pa_box("manifest", None, &store(), &[])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn update_with_the_c2pa_rs_merkle_offset_is_located() {
    // Regression: `c2pa-rs` writes 8 zero-filled merkle-offset bytes ahead of an `update` store just
    // as it does for `manifest`/`original`, so this is the layout of real mid-update files. Reading
    // the `LBox` at offset 0 would find those zeros and report nothing at all.
    let store = store();
    let data = file_with(&[c2pa_box("update", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c
        .c2pa()
        .expect("update store located past the merkle offset");
    assert_eq!(found.purpose, C2paBoxPurpose::Update);
    // 16 (ftyp) + 8 (header) + 16 (user type) + 4 (version+flags) + 7 ("update\0") = 51 for `data`,
    // then 8 for the merkle offset = 59.
    assert_eq!(found.range, 59..88);
    assert_eq!(found.bytes, store.as_slice());
    assert_eq!(&data[found.range.clone()], store.as_slice());
    // The eight bytes before the store are the zero-filled offset, outside the range.
    assert_eq!(&data[51..59], &[0u8; 8]);
}

#[test]
fn update_without_a_merkle_offset_is_located_by_the_fallback_probe() {
    // The specification-literal layout: the store begins immediately after the purpose string. The
    // probe tries offset 8 first; in *this* store that lands on the ASCII interior, which reads as a
    // length far past the end, so it falls back to offset 0. That fall-through is a property of this
    // fixture's contents, not a guarantee — offset 8 is past both `LBox` and `TBox`, so on a real
    // superbox it lands on the first interior box's own length and can read as a valid bound. See
    // `C2paBoxPurpose`; this is the documented content-dependent limit of the probe.
    let store = store();
    let data = file_with(&[c2pa_box("update", None, &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("update store located at the start of data");
    assert_eq!(found.purpose, C2paBoxPurpose::Update);
    assert_eq!(found.range, 51..80);
    assert_eq!(found.bytes, store.as_slice());
    assert_eq!(&data[found.range.clone()], store.as_slice());
}

#[test]
fn update_probes_the_merkle_offset_before_the_bare_store() {
    // Probe *order* is load-bearing, not just probe membership. This merkle offset's leading four
    // bytes are 0x00000020 = 32, which is >= the 8-byte JUMBF header and <= the 37 bytes of `data`,
    // so reading an `LBox` at offset 0 yields a "valid" bound over the wrong 32 bytes. Trying offset
    // 8 first is what keeps the real store the one reported.
    //
    // This is also a constructed instance of the general hazard: `LBox` validity alone cannot tell a
    // real store bound from a plausible number in the wrong place, which is why `C2paBoxPurpose`
    // documents the offset-less `update` layout as possibly mis-bounded rather than fail-safe.
    let store = store();
    let data = file_with(&[c2pa_box("update", Some(0x0000_0020_0000_0000), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c
        .c2pa()
        .expect("update store located past the merkle offset");
    assert_eq!(found.range, 59..88);
    assert_eq!(found.bytes, store.as_slice());
    // The decoy bound the reversed order would have produced.
    assert_ne!(found.bytes, &data[51..83]);
}

#[test]
fn update_reports_nothing_when_neither_probe_offset_bounds_a_store() {
    // Both candidates fail: 4 bytes of zero where an `LBox` would sit at offset 0, and nothing but
    // padding at offset 8. When no candidate bounds anything the answer is absence, not a guess.
    let data = file_with(&[c2pa_box("update", None, &[0, 0, 0, 0], &[0xAB; 8])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
}

#[test]
fn mid_update_file_reports_both_stores_in_file_order() {
    // §A.5.3: an `original` box indicates a sibling `update` box. Which one is *active* is a
    // validator's judgement, so both are reported and `c2pa()` promises only the first. Both boxes
    // carry the merkle offset, which is the layout `c2pa-rs` writes for a mid-update file.
    let original = jumbf_store(b"original-store");
    let update = jumbf_store(b"update-store");
    let data = file_with(&[
        c2pa_box("original", Some(0), &original, &[]),
        c2pa_box("update", Some(0), &update, &[]),
    ]);
    let c = HeifContainer::parse(&data).unwrap();

    let all: Vec<_> = c.c2pa_manifest_stores().collect();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].purpose, C2paBoxPurpose::Original);
    assert_eq!(all[0].bytes, original.as_slice());
    assert_eq!(all[1].purpose, C2paBoxPurpose::Update);
    assert_eq!(all[1].bytes, update.as_slice());
    // File order, and the second range starts after the first ends.
    assert!(all[0].range.end < all[1].range.start);
    assert_eq!(&data[all[1].range.clone()], update.as_slice());
    assert_eq!(c.c2pa().expect("first store"), all[0]);
}

#[test]
fn the_summary_carries_every_located_store_with_its_range_size_and_purpose() {
    let original = jumbf_store(b"original-store");
    let update = jumbf_store(b"update-store-that-is-longer");
    let data = file_with(&[
        c2pa_box("original", Some(0), &original, &[]),
        c2pa_box("update", Some(0), &update, &[]),
    ]);
    let c = HeifContainer::parse(&data).unwrap();

    let located: Vec<_> = c.c2pa_manifest_stores().collect();
    let summary = c.c2pa_summary();
    assert!(summary.is_present());
    assert_eq!(summary.stores.len(), located.len());
    for (reported, found) in summary.stores.iter().zip(&located) {
        assert_eq!(reported.range, found.range);
        assert_eq!(reported.purpose, found.purpose);
        assert_eq!(reported.size(), found.bytes.len());
    }
    // The two stores differ in size, so a summary built from the wrong store is visible here.
    assert_ne!(summary.stores[0].size(), summary.stores[1].size());
}

#[test]
fn a_report_line_never_carries_a_stores_bytes() {
    // C2PA 2.4 §15.12: a store is opaque to gamut, and rendering it would invite the reading that
    // gamut understands — and so has checked — the manifest. A byte range is the whole report.
    let contents = b"MANIFEST-STORE-CONTENTS";
    let data = file_with(&[c2pa_box("manifest", Some(0), &jumbf_store(contents), &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let lines = c.c2pa_summary().report_lines();
    let rendered = lines.join("\n");
    assert!(
        !rendered.contains(std::str::from_utf8(contents).unwrap()),
        "the store's contents leaked into the report: {rendered}"
    );
    // Nor the JUMBF framing that bounds them.
    assert!(
        !rendered.contains("jumb"),
        "the store's header leaked: {rendered}"
    );
}

#[test]
fn a_file_with_no_c2pa_box_summarises_as_absent() {
    let data = file_with(&[]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(!summary.is_present());
    assert!(summary.stores.is_empty());
    assert_eq!(summary.report_lines().len(), 1);
}

#[test]
fn largesize_header_shifts_the_range_by_its_extra_eight_bytes() {
    // A 64-bit largesize header is 16 bytes, not 8. The offsets are derived from the segment range
    // and the body length, so the store must move by exactly the extra 8 header bytes.
    let inner = c2pa_box("manifest", Some(0), &store(), &[]);
    let body = &inner[8..];
    let mut large = vec![0, 0, 0, 1, b'u', b'u', b'i', b'd'];
    large.extend_from_slice(&((16 + body.len()) as u64).to_be_bytes());
    large.extend_from_slice(body);
    let data = file_with(&[large]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.range, 69..98);
    assert_eq!(found.bytes, store().as_slice());
    assert_eq!(&data[found.range.clone()], store().as_slice());
}

#[test]
fn store_is_trimmed_to_its_lbox_not_to_the_box_length() {
    // The box carries 100 bytes of padding after a 12-byte store; only the store is reported.
    let store = jumbf_store(b"tiny");
    assert_eq!(store.len(), 12);
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[0x5A; 100])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.range, 61..73);
    assert_eq!(found.bytes, store.as_slice());
}

#[test]
fn minimum_lbox_of_exactly_the_jumbf_header_is_accepted() {
    // LBox == 8 is the smallest legal JUMBF box (LBox + TBox, §8.4.2.3) — an empty superbox.
    let store = jumbf_store(b"");
    assert_eq!(store.len(), 8);
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[0x11; 4])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.range, 61..69);
    assert_eq!(found.bytes, store.as_slice());
}

#[test]
fn file_without_a_c2pa_box_reports_none() {
    let data = clean_file(1, vec![hvc1_item(1, vec![1, 2, 3, 4])]);
    let c = HeifContainer::parse(&data).unwrap();

    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
}

#[test]
fn top_level_uuid_with_a_non_c2pa_user_type_is_not_reported() {
    // A vendor `uuid` box whose payload happens to look exactly like a C2PA one: only the extended
    // type (§A.5.1.1) decides, so it must not be reported.
    let mut foreign = C2PA_UUID;
    foreign[0] ^= 0xFF;
    let data = file_with(&[uuid_box(
        &foreign,
        0,
        0,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();

    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
    // It is still accounted for as a top-level box — nothing is dropped.
    assert!(c.boxes().any(|(ty, _)| &ty == b"uuid"));
}

#[test]
fn non_uuid_box_carrying_c2pa_framing_is_not_reported() {
    // §A.5.1.1 fixes the box type as `uuid`; the extended type only qualifies a box that already is
    // one. A vendor box whose body is a byte-for-byte copy of a ContentProvenanceBox body — the C2PA
    // user type, `FullBox` 0/0, `manifest`, merkle offset and a valid store — is not a manifest
    // store, and the box type is the only thing that says so.
    let provenance = c2pa_box("manifest", Some(0), &store(), &[]);
    let disguised = bx(b"mpvd", &provenance[8..]);
    let data = file_with(&[disguised]);
    let c = HeifContainer::parse(&data).unwrap();

    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
    // Still accounted for as a top-level box, exactly as before.
    let mpvd = c
        .boxes()
        .find(|(ty, _)| ty == b"mpvd")
        .expect("mpvd surfaced");
    assert_eq!(mpvd.1, &provenance[8..]);
}

#[test]
fn uuid_inside_meta_is_not_a_manifest_store() {
    // §A.5.3 places the ContentProvenanceBox at the top level. A `meta` child with identical framing
    // is not one — but it is still surfaced verbatim as an unknown meta box.
    let nested = c2pa_box("manifest", Some(0), &store(), &[]);
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iinf_v0(&[infe_v2(1, b"hvc1", false)]),
        nested.clone(),
    ]);
    let data = cat(&[ftyp(b"heic"), m]);
    let c = HeifContainer::parse(&data).unwrap();

    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
    let unknown = c
        .unknown_meta_boxes()
        .iter()
        .find(|b| &b.ty == b"uuid")
        .expect("nested uuid still surfaced");
    assert_eq!(unknown.location, UnknownBoxLocation::Meta);
    assert_eq!(unknown.body, &nested[8..]);
}

#[test]
fn non_zero_full_box_version_is_not_reported() {
    let data = file_with(&[uuid_box(
        &C2PA_UUID,
        1,
        0,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn non_zero_full_box_flags_are_not_reported() {
    let data = file_with(&[uuid_box(
        &C2PA_UUID,
        0,
        1,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn merkle_box_is_not_a_manifest_store() {
    // §A.5.3 lists only `manifest`, `original` and `update` as manifest-store purposes; a `merkle`
    // box holds Merkle-tree hashes, not a store, so it is not reported.
    let data = file_with(&[uuid_box(&C2PA_UUID, 0, 0, "merkle", &store())]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
    assert_eq!(c.c2pa_manifest_stores().count(), 0);
}

#[test]
fn unrecognised_box_purpose_is_not_reported() {
    let data = file_with(&[uuid_box(
        &C2PA_UUID,
        0,
        0,
        "manifesto",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn unterminated_box_purpose_is_not_reported() {
    // No NUL anywhere after the FullBox header: the purpose string never ends.
    let body = cat(&[&C2PA_UUID[..], &[0, 0, 0, 0], b"manifest"]);
    let data = file_with(&[bx(b"uuid", &body)]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn uuid_box_holding_only_the_user_type_is_not_reported() {
    // Exactly 16 bytes of body: the user type matches, but there is no FullBox header at all.
    let data = file_with(&[bx(b"uuid", &C2PA_UUID)]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn data_shorter_than_the_merkle_offset_is_not_reported() {
    // `manifest` promises 8 merkle-offset bytes; only 7 are present, so there is no store.
    let data = file_with(&[uuid_box(&C2PA_UUID, 0, 0, "manifest", &[0; 7])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn store_shorter_than_its_lbox_field_is_not_reported() {
    // Three bytes where a 4-byte LBox must be.
    let data = file_with(&[c2pa_box("manifest", Some(0), &[0, 0, 0], &[])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn zero_lbox_is_not_reported() {
    let mut store = jumbf_store(b"payload");
    store[..4].copy_from_slice(&0u32.to_be_bytes());
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn lbox_below_the_jumbf_header_length_is_not_reported() {
    // Non-zero but smaller than the 8-byte LBox+TBox header it must itself cover (§8.4.2.3).
    let mut store = jumbf_store(b"payload");
    store[..4].copy_from_slice(&7u32.to_be_bytes());
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn lbox_overrunning_the_uuid_box_is_not_reported() {
    let mut store = jumbf_store(b"payload");
    let overrun = (store.len() + 1) as u32;
    store[..4].copy_from_slice(&overrun.to_be_bytes());
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();
    assert!(c.c2pa().is_none());
}

#[test]
fn lbox_exactly_filling_the_remaining_data_is_reported() {
    // The boundary case on the other side of the overrun check: LBox == the bytes available.
    let store = jumbf_store(b"payload");
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.bytes, store.as_slice());
    assert_eq!(found.range, 61..61 + store.len());
}

#[test]
fn a_c2pa_box_with_a_non_zero_full_box_version_is_reported_as_unread() {
    // §A.5.1.2 fixes version and flags at zero, so the box carries no store gamut can read — but it
    // is unmistakably a C2PA box, and reporting nothing would read as "this file has no provenance".
    let data = file_with(&[uuid_box(
        &C2PA_UUID,
        1,
        0,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(summary.stores.is_empty());
    assert_eq!(summary.unread.len(), 1);
    assert_eq!(summary.unread[0].reason, C2paUnreadReason::NotVersionZero);
}

#[test]
fn a_merkle_box_is_reported_as_unread_rather_than_as_nothing_at_all() {
    // §A.5.3 gives `merkle` no manifest store, so there is genuinely none to locate; the box itself
    // is still C2PA framing the file carries.
    let data = file_with(&[uuid_box(&C2PA_UUID, 0, 0, "merkle", &store())]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(summary.stores.is_empty());
    assert_eq!(summary.unread.len(), 1);
    assert_eq!(
        summary.unread[0].reason,
        C2paUnreadReason::NotAManifestStorePurpose
    );
}

#[test]
fn a_c2pa_box_holding_only_the_user_type_is_reported_as_truncated() {
    // Exactly 16 bytes of body: the extended type matches, so it is a C2PA box, and there is no
    // `FullBox` header after it.
    let data = file_with(&[bx(b"uuid", &C2PA_UUID)]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(summary.stores.is_empty());
    assert_eq!(summary.unread.len(), 1);
    assert_eq!(summary.unread[0].reason, C2paUnreadReason::Truncated);
}

#[test]
fn a_c2pa_box_whose_lbox_overruns_it_is_reported_as_unbounded() {
    // A hostile length where the store's own `LBox` must be: the framing is spec-clean up to the
    // store, and only the bound is unusable.
    let mut store = jumbf_store(b"payload");
    store[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    let data = file_with(&[c2pa_box("manifest", Some(0), &store, &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(summary.stores.is_empty());
    assert_eq!(summary.unread.len(), 1);
    assert_eq!(summary.unread[0].reason, C2paUnreadReason::NoStoreBound);
}

#[test]
fn an_unread_boxs_range_covers_the_whole_uuid_box() {
    // The store's own range is unavailable — there is no store — so the whole box is what is
    // reported, header and extended type included, starting right after the 16-byte `ftyp`.
    let inner = uuid_box(&C2PA_UUID, 0, 0, "merkle", &store());
    let data = file_with(std::slice::from_ref(&inner));
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert_eq!(
        summary.unread[0].range,
        AFTER_FTYP..AFTER_FTYP + inner.len()
    );
    assert_eq!(&data[summary.unread[0].range.clone()], inner.as_slice());
}

#[test]
fn a_foreign_uuid_box_is_not_reported_as_an_unread_c2pa_box() {
    // §A.5.1.1 makes the extended type the whole test. An ordinary file carries vendor `uuid`
    // boxes; reporting one byte off the C2PA type as damaged C2PA framing would claim provenance
    // where there is none, which is the same defect as claiming absence where there is some.
    let mut foreign = C2PA_UUID;
    foreign[0] ^= 0xFF;
    let data = file_with(&[uuid_box(
        &foreign,
        0,
        0,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert!(summary.stores.is_empty());
    assert!(summary.unread.is_empty());
}

#[test]
fn a_uuid_box_of_another_extended_type_is_counted_rather_than_passed_over() {
    // The near miss and the file with no `uuid` box at all produced byte-identical reports before
    // this count existed — and a signed file corrupted in transit is precisely the first. The count
    // is a fact about bytes: it neither claims provenance for the box nor calls it damaged C2PA
    // framing, which §A.5.1.1 forbids since the extended type is the box's whole identity.
    let mut foreign = C2PA_UUID;
    foreign[0] ^= 0xFF;
    let data = file_with(&[uuid_box(
        &foreign,
        0,
        0,
        "manifest",
        &cat(&[&0u64.to_be_bytes()[..], &store()]),
    )]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert_eq!(summary.other_uuid_boxes, 1);
    // Still not a store and still not an unread C2PA box: only the count changed.
    assert!(summary.stores.is_empty());
    assert!(summary.unread.is_empty());
}

#[test]
fn a_c2pa_box_is_counted_as_a_c2pa_box_and_never_as_a_uuid_box_of_another_type() {
    // The two tallies partition the top-level `uuid` boxes; a box counted in both, or in the wrong
    // one, would let a reader double-count the provenance framing a file carries.
    let data = file_with(&[c2pa_box("manifest", Some(0), &store(), &[])]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert_eq!(summary.stores.len(), 1);
    assert_eq!(summary.other_uuid_boxes, 0);
}

#[test]
fn a_store_before_the_files_media_data_sits_where_a_5_3_places_it() {
    // §A.5.3: "before the first 'mdat' box in the file and before any 'moov' box in the file".
    let data = cat(&[
        ftyp(b"heic"),
        c2pa_box("manifest", Some(0), &store(), &[]),
        bx(b"mdat", &[0xAA; 16]),
        minimal_meta(),
    ]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert_eq!(summary.stores.len(), 1);
    assert_eq!(summary.stores[0].position, C2paBoxPosition::BeforeMediaData);
}

#[test]
fn a_store_after_the_files_media_data_is_flagged_by_its_position() {
    // The adversarial shape: a `ContentProvenanceBox` appended past the media data rather than
    // written into the window §A.5.3 mandates. Reported as a position, not as a verdict — §A.5.3
    // itself puts a mid-update `update` box last in the file.
    let data = cat(&[
        ftyp(b"heic"),
        bx(b"mdat", &[0xAA; 16]),
        c2pa_box("manifest", Some(0), &store(), &[]),
        minimal_meta(),
    ]);
    let c = HeifContainer::parse(&data).unwrap();

    let summary = c.c2pa_summary();
    assert_eq!(summary.stores.len(), 1);
    assert_eq!(summary.stores[0].position, C2paBoxPosition::AfterMediaData);
}

#[test]
fn a_top_level_moov_never_reaches_the_c2pa_lens() {
    // Why the boundary is the first `mdat` alone, though §A.5.3 names `moov` as well: a top-level
    // movie box is refused by the container before any C2PA scan runs — image sequences are out of
    // scope — so testing for one would be a branch no parsed file could take.
    let data = cat(&[
        ftyp(b"heic"),
        bx(b"moov", &[0xAA; 16]),
        c2pa_box("manifest", Some(0), &store(), &[]),
        minimal_meta(),
    ]);
    let error = HeifContainer::parse(&data).expect_err("a top-level moov is not a still image");
    assert!(
        error.to_string().contains("image sequences"),
        "the container must refuse the file, not classify its boxes: {error}"
    );
}

#[test]
fn a_uuid_box_too_short_to_hold_an_extended_type_never_reaches_the_c2pa_lens() {
    // Why `classify_uuid_box`'s short-body arm is unreachable rather than a classification: the
    // container rejects such a box outright (`gamut_isobmff::BoxReader::next_box`, "truncated uuid
    // user type"), so there is no summary to report it in — the whole file fails to parse.
    for short in [0usize, 15] {
        let data = file_with(&[bx(b"uuid", &vec![0xAB; short])]);
        let error = HeifContainer::parse(&data)
            .expect_err("a uuid box without its complete user type is a parse error");
        assert!(
            error.to_string().contains("uuid user type"),
            "a {short}-byte uuid body must be refused by the box reader: {error}"
        );
    }
}
