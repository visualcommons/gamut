//! HEIF container wiring: `parse` runs `gamut_isobmff::walk_segments` and surfaces its
//! results faithfully.
//!
//! The walk's rules are pinned in `gamut-isobmff`'s own `tests/accounting.rs` -- one
//! implementation, one place that pins it (#436). What stays here is what is specific to this
//! crate: that its container type is wired to the shared walk, plus the format-specific cases
//! below.

mod common;

use common::{c2pa_box, cat, clean_file, hvc1_item, jumbf_store};
use gamut_heic::{HeifContainer, SegmentKind};

/// Asserts the every-byte invariant through the shared predicate,
/// [`gamut_core::segment::covers`], whose own negative cases are pinned once in `gamut-core`
/// (#500).
fn assert_covers(container: &HeifContainer, len: usize) {
    let segs = container.segments();
    assert!(
        gamut_core::segment::covers(segs, len),
        "segments must tile 0..{len}: {segs:?}"
    );
}

/// The top-level box types, in order (the `SegmentKind::Box` segments).
fn box_types(container: &HeifContainer) -> Vec<[u8; 4]> {
    container.boxes().map(|(ty, _)| ty).collect()
}

/// `parse` runs the shared walk and exposes its results through every accessor.
///
/// The walk's own behaviour -- unknown boxes, alternate size headers, appended streams, trailers,
/// and the ftyp+meta tolerance rule -- is pinned once in `gamut-isobmff`'s `tests/accounting.rs`
/// since #436 made it one implementation. What is left for this crate to pin is that
/// `HeifContainer::parse` calls it and hands the results out unmodified, which is what a
/// mis-wiring would break.
#[test]
fn parse_exposes_the_shared_segment_walk() {
    let data = clean_file(1, vec![hvc1_item(1, vec![0xAA, 0xBB, 0xCC, 0xDD])]);
    let c = HeifContainer::parse(&data).unwrap();

    assert_covers(&c, data.len());
    assert_eq!(box_types(&c), vec![*b"ftyp", *b"meta", *b"mdat"]);
    assert!(c.appended_stream().is_none());
    assert!(c.trailer().is_none());
    // Every segment is a Box.
    assert!(
        c.segments()
            .iter()
            .all(|s| matches!(s.kind, SegmentKind::Box { .. }))
    );

    // `data()` returns the exact input buffer it was parsed from (pins the accessor against the
    // `Vec::leak(..)` replacements). Compare by contents *and* pointer identity — it borrows `data`.
    assert_eq!(c.data(), data.as_slice());
    assert_eq!(c.data().as_ptr(), data.as_ptr());
    assert_eq!(c.data().len(), data.len());
}

#[test]
fn c2pa_bearing_file_accounts_for_every_byte() {
    // The C2PA ContentProvenanceBox is an ordinary top-level `uuid` box, so the every-byte invariant
    // must hold over it unchanged — and the located store must sit inside that box's segment.
    let clean = clean_file(1, vec![hvc1_item(1, vec![7, 7, 7, 7])]);
    let store = jumbf_store(b"opaque-manifest-store");
    let provenance = c2pa_box("manifest", Some(0), &store, &[0xEE; 8]);
    let data = cat(&[clean.clone(), provenance.clone()]);
    let c = HeifContainer::parse(&data).unwrap();

    assert_covers(&c, data.len());
    assert_eq!(box_types(&c), vec![*b"ftyp", *b"meta", *b"mdat", *b"uuid"]);
    assert!(c.appended_stream().is_none());
    assert!(c.trailer().is_none());

    let uuid_segment = c.segments().last().unwrap();
    assert_eq!(uuid_segment.range, clean.len()..data.len());

    let found = c.c2pa().expect("manifest store located");
    assert_eq!(found.bytes, store.as_slice());
    // Strictly inside the box segment: the framing before it and the padding after it are still
    // claimed by that segment, so no byte is orphaned or double-counted.
    assert!(uuid_segment.range.start < found.range.start());
    assert!(found.range.end() < uuid_segment.range.end);
    assert_eq!(found.range.bytes_in(&data), Some(store.as_slice()));
}
