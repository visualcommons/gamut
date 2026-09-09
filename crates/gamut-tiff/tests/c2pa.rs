//! The C2PA manifest store in a TIFF: where the encoder puts it, what it reports, and what a
//! reader gets back (C2PA 2.4 §A.3.6, §18.5.5).
//!
//! Placement and exclusion are `gamut_ifd::c2pa`'s and are pinned there; what this file pins is
//! this crate's use of them — that a store survives an encode of a real image, in the right
//! directory, at the end of the file, verbatim.

use gamut_core::{Dimensions, ImageRef, Rgb8};
use gamut_tiff::{
    ByteOrder, SpanKind, TiffDecoder, TiffEncoder, TiffMetadata, c2pa_exclusions, deconstruct,
    read, tags,
};

/// A store whose bytes are neither a palindrome nor a repetition, so a byte-swapped copy of it
/// cannot equal it. §A.3.6 says the TIFF header's `ByteOrder` does **not** govern the store, and
/// only an asymmetric payload can catch a writer that swapped it anyway — which is also why
/// every test here writes big-endian (`MM`), the order where a swap would be invisible under a
/// little-endian default.
const STORE: &[u8] = &[
    0x00, 0x00, 0x00, 0x16, b'j', b'u', b'm', b'b', 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
];

fn rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h * 3).map(|i| (i % 251) as u8).collect()
}

fn image(pixels: &[u8], w: u32, h: u32) -> ImageRef<'_, Rgb8> {
    ImageRef::<Rgb8>::new(
        pixels,
        Dimensions {
            width: w,
            height: h,
        },
    )
    .expect("image")
}

/// A big-endian TIFF carrying `STORE`, plus the encoder's report of where it landed.
fn encoded_with_store() -> (Vec<u8>, gamut_tiff::TiffEncodeReport) {
    let pixels = rgb(17, 13);
    let mut bytes = Vec::new();
    let report = TiffEncoder::new()
        .with_byte_order(ByteOrder::BigEndian)
        .with_metadata(TiffMetadata::new().with_c2pa(STORE.to_vec()))
        .encode_with_report(image(&pixels, 17, 13), &mut bytes)
        .expect("encode");
    (bytes, report)
}

#[test]
fn the_store_is_written_verbatim_into_a_big_endian_file() {
    let (bytes, report) = encoded_with_store();
    let range = report.c2pa.expect("a store was written").store;
    assert_eq!(
        &bytes[range.start as usize..range.end() as usize],
        STORE,
        "the store's bytes are not governed by the file's byte order"
    );
    assert_eq!(
        TiffDecoder::new()
            .metadata(&bytes)
            .expect("metadata")
            .c2pa
            .as_deref(),
        Some(STORE)
    );
}

#[test]
fn the_store_is_the_last_thing_in_the_file() {
    // §A.3.6: the store goes at the end so resizing it moves no other offset. If anything were
    // written after it, a signer replacing the store would invalidate those bytes' offsets.
    let (bytes, report) = encoded_with_store();
    let range = report.c2pa.expect("a store was written").store;
    assert_eq!(range.end(), bytes.len() as u64);
    assert_eq!(range.len, STORE.len() as u64);
}

#[test]
fn the_two_exclusion_ranges_are_disjoint() {
    // §18.5.5 excludes the store *and*, separately, the count field of its IFD entry — two
    // ranges, never one: the count field lives in a directory body and the store outside it.
    let (_, report) = encoded_with_store();
    let excl = report.c2pa.expect("a store was written");
    assert!(
        excl.count_field.end() <= excl.store.start || excl.store.end() <= excl.count_field.start,
        "overlapping exclusion ranges: {excl:?}"
    );
    assert_eq!(excl.count_field.len, 4, "classic TIFF count field");
}

#[test]
fn a_reservation_is_zero_filled_at_the_offset_the_report_gives() {
    // The reserve-then-sign flow: a signer hashes around these ranges and overwrites the
    // reservation in place, so the reservation must be exactly `len` bytes where the report says.
    let pixels = rgb(17, 13);
    let mut bytes = Vec::new();
    let report = TiffEncoder::new()
        .with_byte_order(ByteOrder::BigEndian)
        .with_c2pa_reserved(64)
        .encode_with_report(image(&pixels, 17, 13), &mut bytes)
        .expect("encode");
    let range = report.c2pa.expect("a reservation was written").store;
    assert_eq!(range.len, 64);
    assert_eq!(&bytes[range.start as usize..range.end() as usize], &[0; 64]);
}

#[test]
fn the_tile_path_places_the_store_too() {
    // Tiling builds its own directory, so it needs its own claim: a tiled encode that forgot to
    // reserve the entry would still pass every strip-path test.
    let pixels = rgb(32, 32);
    let mut bytes = Vec::new();
    let report = TiffEncoder::new()
        .with_byte_order(ByteOrder::BigEndian)
        .with_tiling(16, 16)
        .with_metadata(TiffMetadata::new().with_c2pa(STORE.to_vec()))
        .encode_with_report(image(&pixels, 32, 32), &mut bytes)
        .expect("encode");
    let range = report.c2pa.expect("a store was written").store;
    assert_eq!(&bytes[range.start as usize..range.end() as usize], STORE);
}

#[test]
fn a_multipage_document_puts_the_entry_in_its_last_page() {
    // §A.3.6: one store for the whole asset, in the *last* IFD of the main chain — not page 0,
    // where this crate's other metadata goes.
    let pixels = rgb(4, 4);
    let page = image(&pixels, 4, 4);
    let mut bytes = Vec::new();
    TiffEncoder::new()
        .with_byte_order(ByteOrder::BigEndian)
        .with_metadata(TiffMetadata::new().with_c2pa(STORE.to_vec()))
        .encode_pages_rgb8(&[page, page], &mut bytes)
        .expect("encode");
    let file = read(&bytes).expect("read");
    assert_eq!(file.ifds.len(), 2);
    assert_eq!(file.ifds[0].get(tags::C2PA_MANIFEST_STORE), None);
    assert!(file.ifds[1].get(tags::C2PA_MANIFEST_STORE).is_some());
    let range = c2pa_exclusions(&bytes)
        .expect("locate")
        .expect("a store")
        .store;
    assert_eq!(&bytes[range.start as usize..range.end() as usize], STORE);
}

#[test]
fn a_file_without_a_store_has_no_exclusion_ranges() {
    let pixels = rgb(8, 4);
    let mut bytes = Vec::new();
    let report = TiffEncoder::new()
        .encode_with_report(image(&pixels, 8, 4), &mut bytes)
        .expect("encode");
    assert_eq!(report.c2pa, None);
    assert_eq!(report.len, bytes.len());
    assert_eq!(c2pa_exclusions(&bytes).expect("locate"), None);
}

#[test]
fn the_deconstruct_accounts_the_store_and_recognises_its_tag() {
    // The crate's v1 guarantee is zero-tolerance byte accounting. A store appended after the
    // pixel data must come back as its entry's typed value span — not as an unclassified run,
    // not as a trailer — and its private tag must not be reported as unknown.
    let (bytes, report) = encoded_with_store();
    let range = report.c2pa.expect("a store was written").store;
    let deconstructed = deconstruct(&bytes).expect("deconstruct");
    assert!(
        deconstructed.segments.is_fully_classified(),
        "unclassified: {:?}",
        deconstructed.segments.unclassified
    );
    assert!(
        deconstructed.segments.segments.iter().any(|segment| {
            segment.range == range
                && matches!(segment.kind, SpanKind::Value { tag, .. } if tag == tags::C2PA_MANIFEST_STORE)
        }),
        "the store is not claimed as its entry's value: {:?}",
        deconstructed.segments.segments
    );
    assert!(
        !deconstructed
            .segments
            .segments
            .iter()
            .any(|segment| segment.kind == SpanKind::Trailer),
        "the store must be claimed, not left as a trailer"
    );
    assert!(
        deconstructed.unknown_tags.is_empty(),
        "unknown tags: {:?}",
        deconstructed.unknown_tags
    );
}
